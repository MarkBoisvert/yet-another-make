//! The build state file: everything one build hands the next, in one binary file
//! per profile, `target/<Profile>/.yam/state`, plus a journal next to it.
//!
//! - **Contents:** the output record ([`Outputs`]), the scan cache ([`ScanCache`]),
//!   the header record ([`HeaderRecord`]), the directory listings the source walk
//!   reuses ([`DirCache`]) and the discovered toolchain ([`toolchain::Cached`]).
//! - **Layout:** fixed-width little-endian fields. A header (magic, format version,
//!   yam version), then one table of every string, then one section per record, then
//!   a blake3 checksum of everything before it. Strings are referred to by index, so
//!   each path is stored once. The whole file is read with one `fs::read`.
//! - **Snapshot and journal, like ninja's logs:** the snapshot is rewritten (to a
//!   temporary file, then renamed into place) at the end of a build that changed
//!   anything. While a build runs, each finished job's results are appended to
//!   `state.journal` as a framed, checksummed record. The journal names the snapshot
//!   it extends; loading replays it over that snapshot, and ignores a half-written
//!   last record. So a killed build keeps the progress it made.
//! - **Errors:** a missing file is an empty state. A corrupt, truncated or
//!   outdated one is discarded with the reason, and the build starts over.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use super::headers::HeaderRecord;
use super::outputs::{Outputs, Stat};
use super::scan::{Deps, Provided};
use super::scan_cache::{Key, ScanCache};
use crate::manifest::{DirCache, Listing};
use crate::toolchain::{self, ModuleSource, StdModules, Tool, Toolchain, Version};

const SNAPSHOT_MAGIC: &[u8; 8] = b"YAMSTATE";
const JOURNAL_MAGIC: &[u8; 8] = b"YAMJOURN";
/// Bumped whenever the layout of either file changes.
const FORMAT_VERSION: u32 = 2;
const YAM_VERSION: &str = env!("CARGO_PKG_VERSION");
/// How many bytes of a record's blake3 hash the journal keeps as its checksum.
const RECORD_CHECK: usize = 8;

/// Why a state file was discarded.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("couldn't read `{}`", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{}` is corrupt or from another yam version", path.display())]
    Invalid { path: PathBuf },
}

/// One finished job's results, as the journal records them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// An output and the inputs it was built from: path, contents hash and, when
    /// known, the stat that hash belongs to.
    Output {
        output: String,
        command: [u8; 32],
        inputs: Vec<(String, [u8; 32], Option<Stat>)>,
    },
    /// The headers an object was compiled with.
    Headers {
        object: String,
        headers: Vec<String>,
    },
    /// A scan result.
    Scan { key: Key, deps: Deps },
}

/// Everything loaded from the state file, plus the journal being written.
#[derive(Default)]
pub struct State {
    pub outputs: Outputs,
    pub scan: ScanCache,
    pub headers: HeaderRecord,
    pub dirs: DirCache,
    /// The toolchain the last build discovered; see [`Self::set_toolchain`].
    toolchain: Option<toolchain::Cached>,
    toolchain_changed: bool,
    snapshot: PathBuf,
    journal: PathBuf,
    /// The checksum of the snapshot on disk, which the journal names.
    snapshot_id: [u8; 32],
    /// The open journal, once this build has written to it.
    journal_file: Option<File>,
    /// Whether the journal on disk already extends this snapshot, so new records
    /// can be appended to it.
    journal_valid: bool,
    /// Whether the journal had records to replay, so the snapshot must be rewritten.
    replayed: bool,
}

impl State {
    /// Load the state in `dir` (`target/<Profile>/.yam`): the snapshot, then any
    /// journal over it. Anything unusable is dropped, with the reasons returned.
    #[must_use]
    pub fn load(dir: &Path) -> (Self, Vec<StoreError>) {
        let mut state = Self {
            snapshot: dir.join("state"),
            journal: dir.join("state.journal"),
            ..Self::default()
        };
        let mut errors = Vec::new();
        match fs::read(&state.snapshot) {
            Ok(bytes) => match decode_snapshot(&bytes) {
                Some((parts, id)) => {
                    state.apply(parts);
                    state.snapshot_id = id;
                }
                None => errors.push(StoreError::Invalid {
                    path: state.snapshot.clone(),
                }),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => errors.push(StoreError::Read {
                path: state.snapshot.clone(),
                source,
            }),
        }
        if !errors.is_empty() {
            state.mark_changed();
        }

        match fs::read(&state.journal) {
            // A journal for another snapshot, or from another version, doesn't
            // belong to this state and is ignored.
            Ok(bytes) => {
                if let Some(records) = decode_journal(&bytes, &state.snapshot_id) {
                    state.journal_valid = true;
                    state.replayed = !records.is_empty();
                    for record in records {
                        state.replay(record);
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => errors.push(StoreError::Read {
                path: state.journal.clone(),
                source,
            }),
        }
        (state, errors)
    }

    /// The toolchain the last build discovered, if any.
    #[must_use]
    pub fn toolchain(&self) -> Option<&toolchain::Cached> {
        self.toolchain.as_ref()
    }

    /// Replace the cached toolchain, to be saved with the snapshot.
    pub fn set_toolchain(&mut self, cached: toolchain::Cached) {
        if self.toolchain.as_ref() != Some(&cached) {
            self.toolchain = Some(cached);
            self.toolchain_changed = true;
        }
    }

    /// Append the results recorded since the last call to the journal, so they
    /// survive if the build is killed.
    ///
    /// # Errors
    ///
    /// Fails if the journal can't be written.
    pub fn flush(&mut self) -> io::Result<()> {
        let records: Vec<Record> = self
            .outputs
            .take_pending()
            .into_iter()
            .chain(self.headers.take_pending())
            .chain(self.scan.take_pending())
            .collect();
        if records.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        for record in &records {
            frame(&mut bytes, record);
        }
        if self.journal_file.is_none() {
            if let Some(dir) = self.journal.parent() {
                fs::create_dir_all(dir)?;
            }
            let file = if self.journal_valid {
                OpenOptions::new().append(true).open(&self.journal)?
            } else {
                let mut file = File::create(&self.journal)?;
                file.write_all(&journal_header(&self.snapshot_id))?;
                file
            };
            self.journal_valid = true;
            self.journal_file = Some(file);
        }
        if let Some(file) = &mut self.journal_file {
            file.write_all(&bytes)?;
        }
        Ok(())
    }

    /// Write a new snapshot if anything changed, then drop the journal it now
    /// includes. A no-op build writes nothing.
    ///
    /// # Errors
    ///
    /// Fails if the snapshot can't be written.
    pub fn save(&mut self) -> io::Result<()> {
        // Results still in memory are part of the snapshot, not the journal.
        let _ = self.outputs.take_pending();
        let _ = self.headers.take_pending();
        let _ = self.scan.take_pending();
        let changed = self.replayed
            || self.outputs.needs_save()
            || self.scan.needs_save()
            || self.headers.needs_save()
            || self.dirs.needs_save()
            || self.toolchain_changed;
        if !changed {
            return Ok(());
        }
        self.headers.prune();
        let bytes = encode_snapshot(self);
        if let Some(dir) = self.snapshot.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut temp = self.snapshot.as_os_str().to_owned();
        temp.push(".tmp");
        fs::write(&temp, &bytes)?;
        fs::rename(&temp, &self.snapshot)?;

        self.journal_file = None;
        match fs::remove_file(&self.journal) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        self.snapshot_id = checksum(&bytes[..bytes.len() - 32]);
        self.journal_valid = false;
        self.replayed = false;
        self.outputs.saved();
        self.scan.saved();
        self.headers.saved();
        self.dirs.saved();
        self.toolchain_changed = false;
        Ok(())
    }

    fn mark_changed(&mut self) {
        self.outputs.mark_changed();
        self.scan.mark_changed();
        self.headers.mark_changed();
        self.dirs.mark_changed();
        self.toolchain_changed = true;
    }

    fn apply(&mut self, parts: Parts) {
        self.outputs = parts.outputs;
        self.scan = ScanCache::from_entries(parts.scans);
        self.headers = HeaderRecord::from_parts(parts.header_names, parts.headers);
        self.dirs = DirCache::from_listings(parts.dirs);
        self.toolchain = parts.toolchain;
    }

    fn replay(&mut self, record: Record) {
        match record {
            Record::Output {
                output,
                command,
                inputs,
            } => self.outputs.replay(&output, command, &inputs),
            Record::Headers { object, headers } => {
                self.headers.record(Path::new(&object), headers);
            }
            Record::Scan { key, deps } => self.scan.insert(key, deps),
        }
        // Replaying isn't new work for the journal.
        let _ = self.outputs.take_pending();
        let _ = self.headers.take_pending();
        let _ = self.scan.take_pending();
    }
}

/// What a snapshot decodes to.
struct Parts {
    outputs: Outputs,
    scans: HashMap<Key, Deps>,
    header_names: Vec<String>,
    headers: FxHashMap<String, Vec<u32>>,
    dirs: HashMap<String, Listing>,
    toolchain: Option<toolchain::Cached>,
}

fn checksum(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

// --- writing -----------------------------------------------------------------------

/// Little-endian output, with strings interned into a table written ahead of the
/// body.
#[derive(Default)]
struct Writer {
    body: Vec<u8>,
    strings: Vec<String>,
    ids: HashMap<String, u32>,
}

impl Writer {
    fn u8(&mut self, value: u8) {
        self.body.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.body.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.body.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).unwrap_or(u32::MAX));
    }

    fn hash(&mut self, hash: &[u8; 32]) {
        self.body.extend_from_slice(hash);
    }

    fn str(&mut self, value: &str) {
        let id = if let Some(&id) = self.ids.get(value) {
            id
        } else {
            let id = u32::try_from(self.strings.len()).unwrap_or(u32::MAX);
            self.strings.push(value.to_string());
            self.ids.insert(value.to_string(), id);
            id
        };
        self.u32(id);
    }
}

fn encode_snapshot(state: &State) -> Vec<u8> {
    let (outputs, scan, headers, dirs) = (&state.outputs, &state.scan, &state.headers, &state.dirs);
    let mut w = Writer::default();

    let tables = outputs.parts();
    let (paths, hashes, seen, entries) = (tables.paths, tables.hashes, tables.seen, tables.entries);
    w.len(paths.len());
    for path in paths {
        w.str(path);
    }
    w.len(hashes.len());
    for hash in hashes {
        w.hash(hash);
    }
    w.len(seen.len());
    for (&path, &(stat, hash)) in seen {
        w.u32(path);
        w.u64(stat.mtime);
        w.u64(stat.size);
        w.u32(hash);
    }
    w.len(entries.len());
    for (output, (command, inputs)) in entries {
        w.str(output);
        w.hash(command);
        w.len(inputs.len());
        for &(path, hash) in inputs {
            w.u32(path);
            w.u32(hash);
        }
    }

    let scans = scan.entries();
    w.len(scans.len());
    for (key, deps) in scans {
        w.hash(&key.0);
        write_deps(&mut w, deps);
    }

    let records = headers.entries();
    w.len(records.len());
    for (object, ids) in records {
        w.str(object);
        w.len(ids.len());
        for &id in ids {
            w.str(headers.name(id));
        }
    }

    let listings = dirs.listings();
    w.len(listings.len());
    for (dir, listing) in listings {
        w.str(dir);
        w.u64(listing.mtime);
        w.len(listing.entries.len());
        for (name, is_dir) in &listing.entries {
            w.str(name);
            w.u8(u8::from(*is_dir));
        }
    }

    match &state.toolchain {
        Some(cached) => {
            w.u8(1);
            write_toolchain(&mut w, cached);
        }
        None => w.u8(0),
    }

    let mut out = Vec::with_capacity(w.body.len() + 64);
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    put_str(&mut out, YAM_VERSION);
    out.extend_from_slice(
        &u32::try_from(w.strings.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for string in &w.strings {
        put_str(&mut out, string);
    }
    out.extend_from_slice(&w.body);
    let sum = checksum(&out);
    out.extend_from_slice(&sum);
    out
}

fn write_deps(w: &mut Writer, deps: &Deps) {
    match &deps.provides {
        Some(provided) => {
            w.u8(if provided.is_interface { 2 } else { 1 });
            w.str(&provided.name);
        }
        None => w.u8(0),
    }
    w.len(deps.requires.len());
    for module in &deps.requires {
        w.str(module);
    }
}

fn write_toolchain(w: &mut Writer, cached: &toolchain::Cached) {
    let path = |w: &mut Writer, path: &Path| w.str(&path.to_string_lossy());
    let tool = |w: &mut Writer, tool: &Tool| {
        path(w, &tool.path);
        w.u32(tool.version.major);
        w.u32(tool.version.minor);
        w.u32(tool.version.patch);
    };
    let module = |w: &mut Writer, module: &ModuleSource| {
        path(w, &module.source);
        w.len(module.system_include_dirs.len());
        for dir in &module.system_include_dirs {
            path(w, dir);
        }
    };
    let toolchain = &cached.toolchain;
    w.hash(&cached.fingerprint);
    tool(w, &toolchain.cxx);
    tool(w, &toolchain.scan_deps);
    path(w, &toolchain.ar);
    path(w, &toolchain.std_modules.manifest);
    module(w, &toolchain.std_modules.std);
    match &toolchain.std_modules.std_compat {
        Some(compat) => {
            w.u8(1);
            module(w, compat);
        }
        None => w.u8(0),
    }
}

/// A string written inline: its length, then its bytes.
fn put_str(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn journal_header(snapshot_id: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(JOURNAL_MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    put_str(&mut out, YAM_VERSION);
    out.extend_from_slice(snapshot_id);
    out
}

/// Append `record` to `out` as `[length][payload][checksum]`, strings inline.
fn frame(out: &mut Vec<u8>, record: &Record) {
    let mut payload = Vec::new();
    match record {
        Record::Output {
            output,
            command,
            inputs,
        } => {
            payload.push(1);
            put_str(&mut payload, output);
            payload.extend_from_slice(command);
            payload.extend_from_slice(
                &u32::try_from(inputs.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for (path, hash, stat) in inputs {
                put_str(&mut payload, path);
                payload.extend_from_slice(hash);
                match stat {
                    Some(stat) => {
                        payload.push(1);
                        payload.extend_from_slice(&stat.mtime.to_le_bytes());
                        payload.extend_from_slice(&stat.size.to_le_bytes());
                    }
                    None => payload.push(0),
                }
            }
        }
        Record::Headers { object, headers } => {
            payload.push(2);
            put_str(&mut payload, object);
            payload.extend_from_slice(
                &u32::try_from(headers.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for header in headers {
                put_str(&mut payload, header);
            }
        }
        Record::Scan { key, deps } => {
            payload.push(3);
            payload.extend_from_slice(&key.0);
            match &deps.provides {
                Some(provided) => {
                    payload.push(if provided.is_interface { 2 } else { 1 });
                    put_str(&mut payload, &provided.name);
                }
                None => payload.push(0),
            }
            payload.extend_from_slice(
                &u32::try_from(deps.requires.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for module in &deps.requires {
                put_str(&mut payload, module);
            }
        }
    }
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&payload);
    out.extend_from_slice(&checksum(&payload)[..RECORD_CHECK]);
}

// --- reading -----------------------------------------------------------------------

/// Little-endian input over a byte slice. Every read checks the bounds, so a
/// truncated or corrupt file fails to decode rather than panicking.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// A count, sanity-checked against the bytes left, so a corrupt count can't
    /// ask for a huge allocation.
    fn len(&mut self) -> Option<usize> {
        let len = self.u32()? as usize;
        (len <= self.bytes.len() - self.at).then_some(len)
    }

    fn hash(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }

    /// A string written inline.
    fn inline_str(&mut self) -> Option<&'a str> {
        let len = self.len()?;
        std::str::from_utf8(self.take(len)?).ok()
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Reads the body, resolving string ids through the snapshot's string table.
struct Body<'a> {
    reader: Reader<'a>,
    strings: Vec<&'a str>,
}

impl<'a> Body<'a> {
    fn str(&mut self) -> Option<&'a str> {
        let id = self.reader.u32()? as usize;
        self.strings.get(id).copied()
    }

    fn deps(&mut self) -> Option<Deps> {
        let provides = match self.reader.u8()? {
            0 => None,
            kind @ (1 | 2) => Some(Provided {
                name: self.str()?.to_string(),
                is_interface: kind == 2,
            }),
            _ => return None,
        };
        let count = self.reader.len()?;
        let requires = (0..count)
            .map(|_| self.str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?;
        Some(Deps { provides, requires })
    }

    fn path(&mut self) -> Option<PathBuf> {
        self.str().map(PathBuf::from)
    }

    fn tool(&mut self) -> Option<Tool> {
        Some(Tool {
            path: self.path()?,
            version: Version {
                major: self.reader.u32()?,
                minor: self.reader.u32()?,
                patch: self.reader.u32()?,
            },
        })
    }

    fn module(&mut self) -> Option<ModuleSource> {
        let source = self.path()?;
        let count = self.reader.len()?;
        let system_include_dirs = (0..count).map(|_| self.path()).collect::<Option<_>>()?;
        Some(ModuleSource {
            source,
            system_include_dirs,
        })
    }

    fn toolchain(&mut self) -> Option<toolchain::Cached> {
        let fingerprint = self.reader.hash()?;
        let cxx = self.tool()?;
        let scan_deps = self.tool()?;
        let ar = self.path()?;
        let manifest = self.path()?;
        let std = self.module()?;
        let std_compat = match self.reader.u8()? {
            0 => None,
            1 => Some(self.module()?),
            _ => return None,
        };
        Some(toolchain::Cached {
            fingerprint,
            toolchain: Toolchain {
                cxx,
                scan_deps,
                ar,
                std_modules: StdModules {
                    manifest,
                    std,
                    std_compat,
                },
            },
        })
    }
}

/// Check the magic, version and yam version at the start of a file.
fn read_header(reader: &mut Reader<'_>, magic: [u8; 8]) -> Option<()> {
    (reader.take(8)? == &magic[..]).then_some(())?;
    (reader.u32()? == FORMAT_VERSION).then_some(())?;
    (reader.inline_str()? == YAM_VERSION).then_some(())
}

/// The snapshot's contents and its checksum, or `None` if it's corrupt, truncated
/// or from another version.
fn decode_snapshot(bytes: &[u8]) -> Option<(Parts, [u8; 32])> {
    let split = bytes.len().checked_sub(32)?;
    let (data, sum) = bytes.split_at(split);
    let id = checksum(data);
    (id[..] == *sum).then_some(())?;

    let mut reader = Reader::new(data);
    read_header(&mut reader, *SNAPSHOT_MAGIC)?;
    let count = reader.len()?;
    let strings = (0..count)
        .map(|_| reader.inline_str())
        .collect::<Option<Vec<_>>>()?;
    let mut b = Body { reader, strings };

    let count = b.reader.len()?;
    let paths = (0..count)
        .map(|_| b.str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    let count = b.reader.len()?;
    let hashes = (0..count)
        .map(|_| b.reader.hash())
        .collect::<Option<Vec<_>>>()?;
    let count = b.reader.len()?;
    let mut seen = HashMap::with_capacity(count);
    for _ in 0..count {
        let path = b.reader.u32()?;
        let stat = Stat {
            mtime: b.reader.u64()?,
            size: b.reader.u64()?,
        };
        seen.insert(path, (stat, b.reader.u32()?));
    }
    let count = b.reader.len()?;
    let mut entries = HashMap::with_capacity(count);
    for _ in 0..count {
        let output = b.str()?.to_string();
        let command = b.reader.hash()?;
        let inputs = b.reader.len()?;
        let inputs = (0..inputs)
            .map(|_| Some((b.reader.u32()?, b.reader.u32()?)))
            .collect::<Option<Vec<_>>>()?;
        entries.insert(output, (command, inputs));
    }
    let outputs = Outputs::from_parts(paths, hashes, seen, entries)?;

    let count = b.reader.len()?;
    let mut scans = HashMap::with_capacity(count);
    for _ in 0..count {
        let key = Key(b.reader.hash()?);
        scans.insert(key, b.deps()?);
    }

    // Header names are interned as they're read: each string-table id becomes a
    // header id the first time an object lists it, so no header is copied twice.
    let count = b.reader.len()?;
    let mut headers = FxHashMap::with_capacity_and_hasher(count, rustc_hash::FxBuildHasher);
    let mut header_names: Vec<String> = Vec::new();
    let mut header_of = vec![u32::MAX; b.strings.len()];
    for _ in 0..count {
        let object = b.str()?.to_string();
        let len = b.reader.len()?;
        let mut ids = Vec::with_capacity(len);
        for _ in 0..len {
            let string = b.reader.u32()? as usize;
            let slot = header_of.get_mut(string)?;
            if *slot == u32::MAX {
                *slot = u32::try_from(header_names.len()).ok()?;
                header_names.push(b.strings[string].to_string());
            }
            ids.push(*slot);
        }
        headers.insert(object, ids);
    }

    let count = b.reader.len()?;
    let mut dirs = HashMap::with_capacity(count);
    for _ in 0..count {
        let dir = b.str()?.to_string();
        let mtime = b.reader.u64()?;
        let len = b.reader.len()?;
        let entries = (0..len)
            .map(|_| Some((b.str()?.to_string(), b.reader.u8()? != 0)))
            .collect::<Option<Vec<_>>>()?;
        dirs.insert(dir, Listing { mtime, entries });
    }

    let toolchain = match b.reader.u8()? {
        0 => None,
        1 => Some(b.toolchain()?),
        _ => return None,
    };

    b.reader.done().then_some(())?;
    Some((
        Parts {
            outputs,
            scans,
            header_names,
            headers,
            dirs,
            toolchain,
        },
        id,
    ))
}

/// The journal's records, if it extends the snapshot with checksum `snapshot_id`.
/// Reading stops at the first incomplete or damaged record: a build killed while
/// writing one leaves it half-written.
fn decode_journal(bytes: &[u8], snapshot_id: &[u8; 32]) -> Option<Vec<Record>> {
    let mut reader = Reader::new(bytes);
    read_header(&mut reader, *JOURNAL_MAGIC)?;
    (reader.hash()? == *snapshot_id).then_some(())?;
    let mut records = Vec::new();
    while let Some(record) = next_record(&mut reader) {
        records.push(record);
    }
    Some(records)
}

fn next_record(reader: &mut Reader<'_>) -> Option<Record> {
    let len = reader.len()?;
    let payload = reader.take(len)?;
    let check = reader.take(RECORD_CHECK)?;
    (checksum(payload)[..RECORD_CHECK] == *check).then_some(())?;

    let mut r = Reader::new(payload);
    let record = match r.u8()? {
        1 => {
            let output = r.inline_str()?.to_string();
            let command = r.hash()?;
            let count = r.len()?;
            let mut inputs = Vec::with_capacity(count);
            for _ in 0..count {
                let path = r.inline_str()?.to_string();
                let hash = r.hash()?;
                let stat = match r.u8()? {
                    0 => None,
                    1 => Some(Stat {
                        mtime: r.u64()?,
                        size: r.u64()?,
                    }),
                    _ => return None,
                };
                inputs.push((path, hash, stat));
            }
            Record::Output {
                output,
                command,
                inputs,
            }
        }
        2 => {
            let object = r.inline_str()?.to_string();
            let count = r.len()?;
            let headers = (0..count)
                .map(|_| r.inline_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            Record::Headers { object, headers }
        }
        3 => {
            let key = Key(r.hash()?);
            let provides = match r.u8()? {
                0 => None,
                kind @ (1 | 2) => Some(Provided {
                    name: r.inline_str()?.to_string(),
                    is_interface: kind == 2,
                }),
                _ => return None,
            };
            let count = r.len()?;
            let requires = (0..count)
                .map(|_| r.inline_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            Record::Scan {
                key,
                deps: Deps { provides, requires },
            }
        }
        _ => return None,
    };
    r.done().then_some(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::outputs::CommandHash;
    use crate::engine::scan_cache::{FlagsHash, Key};
    use std::ffi::OsStr;

    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        fn new() -> Self {
            let project = Self {
                dir: tempfile::tempdir().unwrap(),
            };
            fs::write(project.path("a.cpp"), "int a();").unwrap();
            fs::write(project.path("a.o"), "object").unwrap();
            project
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn state_dir(&self) -> PathBuf {
            self.path(".yam")
        }

        fn load(&self) -> (State, Vec<StoreError>) {
            State::load(&self.state_dir())
        }
    }

    fn command() -> CommandHash {
        CommandHash::new("clang 23", [OsStr::new("cc")])
    }

    fn key(contents: &[u8]) -> Key {
        let flags = FlagsHash::new("scan-deps 23", [OsStr::new("-std=c++26")]);
        Key::new(&flags, &blake3::hash(contents), &[])
    }

    fn deps() -> Deps {
        Deps {
            provides: Some(Provided {
                name: "a:part".into(),
                is_interface: false,
            }),
            requires: vec!["std".into(), "b".into()],
        }
    }

    fn cached_toolchain() -> toolchain::Cached {
        let tool = |path: &str| Tool {
            path: path.into(),
            version: Version {
                major: 22,
                minor: 1,
                patch: 8,
            },
        };
        let module = |source: &str| ModuleSource {
            source: source.into(),
            system_include_dirs: vec!["/llvm/share/libc++/v1".into()],
        };
        toolchain::Cached {
            fingerprint: [9; 32],
            toolchain: Toolchain {
                cxx: tool("/llvm/bin/clang++"),
                scan_deps: tool("/llvm/bin/clang-scan-deps"),
                ar: "/llvm/bin/llvm-ar".into(),
                std_modules: StdModules {
                    manifest: "/llvm/lib/libc++.modules.json".into(),
                    std: module("/llvm/share/libc++/v1/std.cppm"),
                    std_compat: Some(module("/llvm/share/libc++/v1/std.compat.cppm")),
                },
            },
        }
    }

    /// One of everything the state holds.
    fn fill(p: &Project, state: &mut State) {
        let inputs = [p.path("a.cpp")];
        assert!(!state.outputs.is_fresh(&p.path("a.o"), command(), &inputs));
        state.outputs.record(&p.path("a.o"), command(), &inputs);
        state.scan.insert(key(b"a"), deps());
        state.headers.record(
            &p.path("a.o"),
            vec!["src/a.hpp".into(), "include/b.hpp".into()],
        );
        state.set_toolchain(cached_toolchain());
        let mut tree_cache = std::mem::take(&mut state.dirs);
        crate::manifest::SourceTree::scan_cached(p.dir.path(), &mut tree_cache).unwrap();
        state.dirs = tree_cache;
    }

    fn assert_filled(p: &Project, state: &mut State) {
        assert!(
            state
                .outputs
                .is_fresh(&p.path("a.o"), command(), &[p.path("a.cpp")])
        );
        assert_eq!(state.scan.get(key(b"a")), Some(deps()));
        assert_eq!(
            state.headers.names_of(&p.path("a.o")),
            ["src/a.hpp", "include/b.hpp"]
        );
        assert_eq!(state.toolchain(), Some(&cached_toolchain()));
        // The loaded listings serve the next walk.
        assert!(state.dirs.needs_save(), "loaded listings wait to be used");
        let tree = crate::manifest::SourceTree::scan_cached(p.dir.path(), &mut state.dirs).unwrap();
        assert_eq!(tree.files(), ["a.cpp", "a.o"]);
        assert!(state.dirs.listings().contains_key(""));
    }

    #[test]
    fn everything_round_trips() {
        let p = Project::new();
        let (mut state, errors) = p.load();
        assert!(errors.is_empty());
        fill(&p, &mut state);
        state.save().unwrap();
        assert!(p.state_dir().join("state").is_file());
        assert!(!p.state_dir().join("state.journal").exists());

        let (mut state, errors) = p.load();
        assert!(errors.is_empty(), "{errors:?}");
        assert_filled(&p, &mut state);
    }

    #[test]
    fn only_a_different_toolchain_needs_saving() {
        let p = Project::new();
        let (mut state, _) = p.load();
        state.set_toolchain(cached_toolchain());
        state.save().unwrap();

        let (mut state, _) = p.load();
        state.set_toolchain(cached_toolchain());
        assert!(!state.toolchain_changed);
        let mut other = cached_toolchain();
        other.fingerprint = [8; 32];
        state.set_toolchain(other.clone());
        assert!(state.toolchain_changed);
        state.save().unwrap();
        assert_eq!(p.load().0.toolchain(), Some(&other));
    }

    #[test]
    fn damaged_files_are_discarded() {
        let p = Project::new();
        let (mut state, _) = p.load();
        fill(&p, &mut state);
        state.save().unwrap();
        let file = p.state_dir().join("state");
        let good = fs::read(&file).unwrap();

        let mut flipped = good.clone();
        flipped[good.len() / 2] ^= 1;
        let mut other_version = good.clone();
        other_version[8] = other_version[8].wrapping_add(1);
        for bad in [
            flipped,
            good[..good.len() - 1].to_vec(),
            good[..20].to_vec(),
            Vec::new(),
            other_version,
        ] {
            fs::write(&file, &bad).unwrap();
            let (mut state, errors) = p.load();
            assert!(
                matches!(errors.as_slice(), [StoreError::Invalid { .. }]),
                "{errors:?}"
            );
            assert_eq!(state.scan.get(key(b"a")), None);
            // A discarded state is written fresh at the end of the build.
            state.save().unwrap();
            assert!(p.load().1.is_empty());
        }
    }

    #[test]
    fn a_killed_build_keeps_what_it_journaled() {
        let p = Project::new();
        let (mut state, _) = p.load();
        fill(&p, &mut state);
        state.save().unwrap();

        // The next build records new results and journals them, then is killed
        // before it saves.
        let (mut state, _) = p.load();
        state.scan.insert(key(b"new"), Deps::default());
        state
            .headers
            .record(&p.path("a.o"), vec!["src/c.hpp".into()]);
        state.flush().unwrap();
        state.scan.insert(key(b"second"), Deps::default());
        state.flush().unwrap();
        drop(state);
        assert!(p.state_dir().join("state.journal").is_file());

        let (mut state, errors) = p.load();
        assert!(errors.is_empty());
        assert_eq!(state.scan.get(key(b"new")), Some(Deps::default()));
        assert_eq!(state.scan.get(key(b"second")), Some(Deps::default()));
        assert_eq!(state.headers.names_of(&p.path("a.o")), ["src/c.hpp"]);

        // Saving folds the journal into the snapshot.
        state.save().unwrap();
        assert!(!p.state_dir().join("state.journal").exists());
        let (mut state, _) = p.load();
        assert_eq!(state.scan.get(key(b"new")), Some(Deps::default()));
    }

    #[test]
    fn a_half_written_record_is_ignored() {
        let p = Project::new();
        let (mut state, _) = p.load();
        state.scan.insert(key(b"whole"), Deps::default());
        state.flush().unwrap();
        state.scan.insert(key(b"cut"), deps());
        state.flush().unwrap();
        drop(state);

        // The build was killed partway through writing the last record.
        let journal = p.state_dir().join("state.journal");
        let bytes = fs::read(&journal).unwrap();
        fs::write(&journal, &bytes[..bytes.len() - 3]).unwrap();
        let (mut state, errors) = p.load();
        assert!(errors.is_empty());
        assert_eq!(state.scan.get(key(b"whole")), Some(Deps::default()));
        assert_eq!(state.scan.get(key(b"cut")), None);
    }

    #[test]
    fn a_journal_for_another_snapshot_is_ignored() {
        let p = Project::new();
        let (mut state, _) = p.load();
        state.scan.insert(key(b"old"), Deps::default());
        state.flush().unwrap();
        drop(state);
        let old_journal = fs::read(p.state_dir().join("state.journal")).unwrap();

        // A snapshot is written; the old journal comes back, as it would if the
        // build was killed between writing the snapshot and removing the journal.
        let (mut state, _) = p.load();
        state.scan.insert(key(b"kept"), Deps::default());
        state.save().unwrap();
        fs::write(p.state_dir().join("state.journal"), old_journal).unwrap();
        let (mut state, errors) = p.load();
        assert!(errors.is_empty());
        assert_eq!(state.scan.get(key(b"kept")), Some(Deps::default()));
        assert_eq!(
            state.scan.get(key(b"old")),
            Some(Deps::default()),
            "the old snapshot's results were saved with it"
        );
    }

    #[test]
    fn records_frame_and_read_back() {
        let records = [
            Record::Output {
                output: "/p/a.o".into(),
                command: [7; 32],
                inputs: vec![
                    ("/p/a.cpp".into(), [1; 32], Some(Stat { mtime: 5, size: 6 })),
                    ("/p/b.pcm".into(), [2; 32], None),
                ],
            },
            Record::Headers {
                object: "/p/a.o".into(),
                headers: vec!["src/a.hpp".into()],
            },
            Record::Scan {
                key: key(b"a"),
                deps: deps(),
            },
        ];
        let mut bytes = journal_header(&[0; 32]);
        for record in &records {
            frame(&mut bytes, record);
        }
        assert_eq!(decode_journal(&bytes, &[0; 32]).unwrap(), records);
        assert_eq!(decode_journal(&bytes, &[1; 32]), None);
    }
}
