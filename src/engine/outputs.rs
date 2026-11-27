//! The output record: what each output (object, archive, executable) was built from,
//! so an unchanged one isn't rebuilt.
//!
//! An output is stale if it's missing, if its command changed, or if any input's
//! contents differ from the ones it was built from. Inputs are the source, the
//! headers it was compiled with (#23), the compiled module interfaces it imported,
//! and for archives and links their objects and libraries.
//!
//! Each input is compared by `stat` first: if its mtime and size match the last time
//! yam saw the file, its recorded contents hash is reused and the file isn't read.
//! Only a file whose mtime or size moved is read and hashed with blake3. So a no-op
//! build is `stat` calls alone, and `touch`, `git checkout` or a save without changes
//! rebuilds nothing. Comparing contents rather than times also gives ninja's
//! `restat` for free: a recompiled interface whose `.pcm` comes out byte-identical
//! leaves its importers fresh.
//!
//! The record is part of the build state file ([`super::State`]). Paths and hashes
//! are interned, so each input of each output is two small numbers.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rustc_hash::FxHashMap;

use super::state::Record;

/// A hash of an output's whole command, including the tools that run it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandHash([u8; 32]);

impl CommandHash {
    /// Hash `tools` (the compiler's path and version, say) and the command's
    /// arguments, program first.
    #[must_use]
    pub fn new<'a>(tools: &str, arguments: impl IntoIterator<Item = &'a OsStr>) -> Self {
        let mut hasher = CommandHasher::new(tools);
        for argument in arguments {
            hasher.arg(argument);
        }
        hasher.finish()
    }
}

/// A [`CommandHash`] built up an argument at a time. Cloning one part way through
/// lets the arguments many commands share be hashed once: every compile in a target
/// starts the same way, and only its last few arguments differ.
#[derive(Clone, Debug)]
pub struct CommandHasher(blake3::Hasher);

impl CommandHasher {
    #[must_use]
    pub fn new(tools: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"yam-command\0");
        hasher.update(tools.as_bytes());
        Self(hasher)
    }

    /// Add one argument.
    pub fn arg(&mut self, argument: impl AsRef<OsStr>) -> &mut Self {
        self.0.update(b"\0");
        self.0.update(argument.as_ref().as_encoded_bytes());
        self
    }

    /// Add arguments already encoded the way [`Self::arg`] does, each one after a
    /// NUL. A constant run of arguments is one literal this way: one hasher call,
    /// not two per argument.
    pub fn encoded(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    #[must_use]
    pub fn finish(&self) -> CommandHash {
        CommandHash(*self.0.finalize().as_bytes())
    }
}

/// A file's mtime (nanoseconds since the epoch) and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub mtime: u64,
    pub size: u64,
}

impl Stat {
    fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(Self {
            mtime: u64::try_from(mtime).unwrap_or(u64::MAX),
            size: meta.len(),
        })
    }
}

/// An output's command hash and `(path id, hash id)` per input.
pub(super) type Entry = ([u8; 32], Vec<(u32, u32)>);

/// The record's tables, as the state file writes them.
pub(super) struct Tables<'a> {
    pub paths: &'a [String],
    pub hashes: &'a [[u8; 32]],
    /// By path id: the stat and contents hash id yam last saw.
    pub seen: &'a HashMap<u32, (Stat, u32)>,
    /// By output path.
    pub entries: &'a HashMap<String, Entry>,
}

/// What this build knows of a file. Each file is `stat`ed at most once a build,
/// whether its existence or its contents is asked about first.
#[derive(Clone, Copy, Debug, Default)]
enum Current {
    #[default]
    Unchecked,
    /// It doesn't exist, or can't be `stat`ed.
    Missing,
    /// Its stat; its contents aren't needed yet.
    Stat(Stat),
    /// It exists but can't be read.
    Unreadable,
    /// Its contents hash id.
    Hash(u32),
}

/// The output record, loaded from the last build and updated by this one.
#[derive(Default)]
pub struct Outputs {
    paths: Vec<String>,
    hashes: Vec<[u8; 32]>,
    /// What yam last saw of each file, by path id: its stat and contents hash id.
    seen: HashMap<u32, (Stat, u32)>,
    /// By output path.
    entries: HashMap<String, Entry>,
    path_ids: FxHashMap<String, u32>,
    hash_ids: FxHashMap<[u8; 32], u32>,
    /// By path id: each file's contents as of this build, so each file is checked
    /// once.
    current: Vec<Current>,
    changed: bool,
    /// Records for the journal since it was last written.
    pending: Vec<Record>,
}

impl Outputs {
    /// A record from its tables, as the state file stores them. `None` if an id is
    /// out of range.
    pub(super) fn from_parts(
        paths: Vec<String>,
        hashes: Vec<[u8; 32]>,
        seen: HashMap<u32, (Stat, u32)>,
        entries: HashMap<String, Entry>,
    ) -> Option<Self> {
        let path_ok = |id: u32| (id as usize) < paths.len();
        let hash_ok = |id: u32| (id as usize) < hashes.len();
        let valid = seen.iter().all(|(&p, &(_, h))| path_ok(p) && hash_ok(h))
            && entries
                .values()
                .all(|(_, inputs)| inputs.iter().all(|&(p, h)| path_ok(p) && hash_ok(h)));
        if !valid {
            return None;
        }
        Some(Self {
            path_ids: index(&paths),
            hash_ids: hashes
                .iter()
                .enumerate()
                .map(|(i, hash)| (*hash, u32::try_from(i).unwrap_or(u32::MAX)))
                .collect(),
            paths,
            hashes,
            seen,
            entries,
            ..Self::default()
        })
    }

    /// The tables, for the state file.
    pub(super) fn parts(&self) -> Tables<'_> {
        Tables {
            paths: &self.paths,
            hashes: &self.hashes,
            seen: &self.seen,
            entries: &self.entries,
        }
    }

    pub(super) fn needs_save(&self) -> bool {
        self.changed
    }

    pub(super) fn saved(&mut self) {
        self.changed = false;
    }

    pub(super) fn mark_changed(&mut self) {
        self.changed = true;
    }

    pub(super) fn take_pending(&mut self) -> Vec<Record> {
        std::mem::take(&mut self.pending)
    }

    /// Apply a journal record: `output` was built by `command` from `inputs`.
    pub(super) fn replay(
        &mut self,
        output: &str,
        command: [u8; 32],
        inputs: &[(String, [u8; 32], Option<Stat>)],
    ) {
        let mut recorded = Vec::with_capacity(inputs.len());
        for (path, hash, stat) in inputs {
            let path = self.intern(Path::new(path));
            let hash = self.hash_id(hash);
            if let Some(stat) = stat {
                self.seen.insert(path, (*stat, hash));
            }
            recorded.push((path, hash));
        }
        self.entries.insert(output.to_string(), (command, recorded));
        self.changed = true;
    }

    /// Whether `output` exists and was built by `command` from `inputs` as they are
    /// now. Every input is checked (and remembered for [`Self::record`]), even after
    /// one is found changed, so a rebuild records the inputs as they were before it
    /// ran.
    pub fn is_fresh<P: AsRef<Path>>(
        &mut self,
        output: &Path,
        command: CommandHash,
        inputs: impl IntoIterator<Item = P>,
    ) -> bool {
        let ids: Vec<u32> = inputs
            .into_iter()
            .map(|input| self.intern(input.as_ref()))
            .collect();
        let output = self.intern(output);
        self.is_fresh_ids(output, command, &ids)
    }

    /// [`Self::is_fresh`] for an output and inputs already interned with
    /// [`Self::intern`].
    pub fn is_fresh_ids(&mut self, output: u32, command: CommandHash, inputs: &[u32]) -> bool {
        let current: Vec<Option<u32>> = inputs.iter().map(|&id| self.current_hash(id)).collect();
        let Some((recorded_command, recorded)) = self.entries.get(&self.paths[output as usize])
        else {
            return false;
        };
        let matches = *recorded_command == command.0
            && recorded.len() == inputs.len()
            && recorded.iter().zip(inputs.iter().zip(&current)).all(
                |(&(path, hash), (&now_path, &now_hash))| {
                    path == now_path && Some(hash) == now_hash
                },
            );
        matches && self.exists(output)
    }

    /// Whether the file exists, as of this build. It shares its `stat` with the
    /// contents check, so a file asked about both ways (an object is both a
    /// compile's output and the archive's input) is `stat`ed once.
    pub fn exists(&mut self, id: u32) -> bool {
        !matches!(self.stat(id), Current::Missing)
    }

    /// `stat` every file the recorded outputs name, inputs and outputs alike, on up
    /// to `threads` threads, so the checks that follow find them already done. On a
    /// no-op build that's nearly every `stat` it makes, and they're most of its
    /// time; the kernel answers them in parallel.
    ///
    /// Only worth calling before anything has been checked. A file a step writes
    /// later is forgotten by [`Self::invalidate`] as usual, so stating early changes
    /// nothing a sequential check wouldn't have seen.
    pub fn prefetch(&mut self, threads: usize) {
        /// Below this many files per thread, starting threads costs more than it
        /// saves.
        const MIN_PER_THREAD: usize = 512;
        /// More threads than this measured no faster.
        const MAX_THREADS: usize = 8;
        let mut wanted = vec![false; self.paths.len()];
        for (output, (_, inputs)) in &self.entries {
            if let Some(&id) = self.path_ids.get(output) {
                wanted[id as usize] = true;
            }
            for &(id, _) in inputs {
                wanted[id as usize] = true;
            }
        }
        let ids: Vec<u32> = (0..self.paths.len())
            .filter(|&i| {
                wanted[i] && matches!(self.current.get(i), None | Some(Current::Unchecked))
            })
            .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
            .collect();
        let threads = threads.min(MAX_THREADS).min(ids.len() / MIN_PER_THREAD);
        if threads < 2 {
            return;
        }
        let paths = &self.paths;
        let stat = |id: &u32| {
            Stat::of(Path::new(&paths[*id as usize])).map_or(Current::Missing, Current::Stat)
        };
        let found: Vec<Vec<Current>> = std::thread::scope(|scope| {
            let workers: Vec<_> = ids
                .chunks(ids.len().div_ceil(threads))
                .map(|chunk| scope.spawn(move || chunk.iter().map(stat).collect()))
                .collect();
            // A worker that panicked leaves its files to be checked as they come up.
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap_or_default())
                .collect()
        });
        self.current.resize(self.paths.len(), Current::Unchecked);
        for (chunk, found) in ids.chunks(ids.len().div_ceil(threads)).zip(found) {
            for (&id, current) in chunk.iter().zip(found) {
                self.current[id as usize] = current;
            }
        }
    }

    /// Record that `output` was built by `command` from `inputs`. Inputs already
    /// checked this build keep the contents seen then; an input that can't be read
    /// isn't recorded, so the output stays stale until it can be.
    pub fn record(&mut self, output: &Path, command: CommandHash, inputs: &[PathBuf]) {
        let mut recorded = Vec::with_capacity(inputs.len());
        let mut complete = true;
        for input in inputs {
            let id = self.intern(input);
            match self.current_hash(id) {
                Some(hash) => recorded.push((id, hash)),
                None => complete = false,
            }
        }
        let key = key(output);
        if !complete {
            self.changed |= self.entries.remove(&key).is_some();
            return;
        }
        let entry = (command.0, recorded);
        if self.entries.get(&key) == Some(&entry) {
            return;
        }
        self.pending.push(Record::Output {
            output: key.clone(),
            command: command.0,
            inputs: entry
                .1
                .iter()
                .map(|&(path, hash)| {
                    let stat = self
                        .seen
                        .get(&path)
                        .filter(|(_, seen)| *seen == hash)
                        .map(|(stat, _)| *stat);
                    (
                        self.paths[path as usize].clone(),
                        self.hashes[hash as usize],
                        stat,
                    )
                })
                .collect(),
        });
        self.entries.insert(key, entry);
        self.changed = true;
    }

    /// Forget what this build knows of `path`, because a step just wrote it.
    pub fn invalidate(&mut self, path: &Path) {
        if let Some(&id) = self.path_ids.get(&key(path))
            && let Some(slot) = self.current.get_mut(id as usize)
        {
            *slot = Current::Unchecked;
        }
    }

    /// A file's contents hash as of this build, or `None` if it can't be read. Like
    /// staleness, this reuses the recorded hash while the file's stat is unchanged,
    /// so an unchanged file isn't read.
    pub fn contents(&mut self, path: &Path) -> Option<blake3::Hash> {
        let id = self.intern(path);
        self.contents_of(id)
    }

    /// [`Self::contents`] for a path already interned with [`Self::intern`].
    pub fn contents_of(&mut self, id: u32) -> Option<blake3::Hash> {
        let hash = self.current_hash(id)?;
        Some(blake3::Hash::from_bytes(self.hashes[hash as usize]))
    }

    /// The record's id for `path`, adding it if it's new. Ids are stable for the
    /// rest of the build, so a caller can look a path up once and keep its id.
    pub fn intern(&mut self, path: &Path) -> u32 {
        // Looked up without allocating; nearly every path is already known.
        if let Some(&id) = path.to_str().and_then(|text| self.path_ids.get(text)) {
            return id;
        }
        let key = key(path);
        if let Some(&id) = self.path_ids.get(&key) {
            return id;
        }
        let id = u32::try_from(self.paths.len()).unwrap_or(u32::MAX);
        self.paths.push(key.clone());
        self.path_ids.insert(key, id);
        self.changed = true;
        id
    }

    fn hash_id(&mut self, hash: &[u8; 32]) -> u32 {
        if let Some(&id) = self.hash_ids.get(hash) {
            return id;
        }
        let id = u32::try_from(self.hashes.len()).unwrap_or(u32::MAX);
        self.hashes.push(*hash);
        self.hash_ids.insert(*hash, id);
        self.changed = true;
        id
    }

    /// The file's contents hash id now: reused from the last time yam saw it when
    /// its stat is unchanged, otherwise read and hashed.
    fn current_hash(&mut self, id: u32) -> Option<u32> {
        let stat = match self.stat(id) {
            Current::Hash(hash) => return Some(hash),
            Current::Missing | Current::Unreadable | Current::Unchecked => return None,
            Current::Stat(stat) => stat,
        };
        let hash = match self.seen.get(&id) {
            Some(&(seen, hash)) if seen == stat => Some(hash),
            _ => fs::read(&self.paths[id as usize]).ok().map(|bytes| {
                let hash = self.hash_id(blake3::hash(&bytes).as_bytes());
                self.seen.insert(id, (stat, hash));
                self.changed = true;
                hash
            }),
        };
        let current = hash.map_or(Current::Unreadable, Current::Hash);
        self.current[id as usize] = current;
        hash
    }

    /// What this build knows of the file, `stat`ing it the first time it's asked
    /// about. Never [`Current::Unchecked`].
    fn stat(&mut self, id: u32) -> Current {
        let slot = id as usize;
        if self.current.len() <= slot {
            self.current.resize(slot + 1, Current::Unchecked);
        }
        if let Current::Unchecked = self.current[slot] {
            self.current[slot] =
                Stat::of(Path::new(&self.paths[slot])).map_or(Current::Missing, Current::Stat);
        }
        self.current[slot]
    }
}

fn index(items: &[String]) -> FxHashMap<String, u32> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.clone(), u32::try_from(i).unwrap_or(u32::MAX)))
        .collect()
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::state::State;
    use std::time::{Duration, SystemTime};

    fn command(text: &str) -> CommandHash {
        CommandHash::new("clang 23", [OsStr::new(text)])
    }

    /// Set a file's mtime, so tests don't depend on the clock's resolution.
    fn set_mtime(path: &Path, seconds: u64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }

    struct Project {
        dir: tempfile::TempDir,
        source: PathBuf,
        header: PathBuf,
        object: PathBuf,
    }

    impl Project {
        fn state_dir(&self) -> PathBuf {
            self.dir.path().join(".yam")
        }

        fn load(&self) -> State {
            let (state, errors) = State::load(&self.state_dir());
            assert!(errors.is_empty(), "{errors:?}");
            state
        }
    }

    fn project() -> Project {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        fs::write(path("a.cpp"), "int a();").unwrap();
        fs::write(path("a.h"), "#pragma once").unwrap();
        fs::write(path("a.o"), "object").unwrap();
        set_mtime(&path("a.cpp"), 1_000);
        set_mtime(&path("a.h"), 1_000);
        Project {
            source: path("a.cpp"),
            header: path("a.h"),
            object: path("a.o"),
            dir,
        }
    }

    /// Build once, then load the saved state as the next build would.
    fn built(p: &Project) -> State {
        let inputs = [p.source.clone(), p.header.clone()];
        let mut state = p.load();
        assert!(!state.outputs.is_fresh(&p.object, command("cc"), &inputs));
        state.outputs.record(&p.object, command("cc"), &inputs);
        state.save().unwrap();
        p.load()
    }

    #[test]
    fn an_unchanged_output_is_fresh_and_writes_nothing() {
        let p = project();
        let mut state = built(&p);
        let inputs = [p.source.clone(), p.header.clone()];
        assert!(state.outputs.is_fresh(&p.object, command("cc"), &inputs));
        fs::remove_file(p.state_dir().join("state")).unwrap();
        state.save().unwrap();
        assert!(
            !p.state_dir().join("state").exists(),
            "a no-op build rewrote it"
        );
    }

    #[test]
    fn a_new_mtime_with_the_same_contents_is_fresh() {
        let p = project();
        let mut state = built(&p);
        set_mtime(&p.header, 2_000);
        let inputs = [p.source.clone(), p.header.clone()];
        assert!(state.outputs.is_fresh(&p.object, command("cc"), &inputs));
    }

    #[test]
    fn changed_contents_commands_inputs_or_outputs_are_stale() {
        let inputs = |p: &Project| [p.source.clone(), p.header.clone()];

        let p = project();
        let mut state = built(&p);
        fs::write(&p.header, "#pragma once\nint x;").unwrap();
        assert!(!state.outputs.is_fresh(&p.object, command("cc"), inputs(&p)));

        let p = project();
        let mut state = built(&p);
        assert!(
            !state
                .outputs
                .is_fresh(&p.object, command("cc -O2"), inputs(&p))
        );
        assert!(
            !state
                .outputs
                .is_fresh(&p.object, command("cc"), &inputs(&p)[..1])
        );

        let p = project();
        let mut state = built(&p);
        fs::remove_file(&p.object).unwrap();
        assert!(!state.outputs.is_fresh(&p.object, command("cc"), inputs(&p)));

        let p = project();
        let mut state = built(&p);
        fs::remove_file(&p.header).unwrap();
        assert!(!state.outputs.is_fresh(&p.object, command("cc"), inputs(&p)));
    }

    #[test]
    fn a_rebuild_records_inputs_as_they_were_when_checked() {
        let p = project();
        let mut state = built(&p);
        let inputs = [p.source.clone(), p.header.clone()];
        fs::write(&p.source, "int a(); // edited").unwrap();
        assert!(!state.outputs.is_fresh(&p.object, command("cc"), &inputs));
        // The source changes again while the compile runs, after it was read.
        fs::write(&p.source, "int a(); // edited twice").unwrap();
        state.outputs.record(&p.object, command("cc"), &inputs);
        state.save().unwrap();

        let mut next = p.load();
        assert!(
            !next.outputs.is_fresh(&p.object, command("cc"), &inputs),
            "the second edit wasn't compiled, so the next build must rebuild"
        );
    }

    #[test]
    fn an_output_written_this_build_is_checked_again() {
        let p = project();
        let mut state = built(&p);
        let inputs = [p.source.clone(), p.header.clone()];
        assert!(state.outputs.is_fresh(&p.object, command("cc"), &inputs));
        fs::write(&p.header, "#pragma once\n// rewritten by a step").unwrap();
        state.outputs.invalidate(&p.header);
        assert!(!state.outputs.is_fresh(&p.object, command("cc"), &inputs));
    }

    #[test]
    fn a_deleted_output_is_stale_and_its_stat_is_shared() {
        let p = project();
        let mut state = built(&p);
        let inputs = [p.source.clone(), p.header.clone()];
        let object = state.outputs.intern(&p.object);
        assert!(state.outputs.exists(object));
        fs::remove_file(&p.object).unwrap();
        // This build already saw it: one `stat` a file, whichever check asks first.
        assert!(state.outputs.is_fresh(&p.object, command("cc"), &inputs));
        let mut state = p.load();
        assert!(!state.outputs.is_fresh(&p.object, command("cc"), &inputs));
    }

    #[test]
    fn a_command_hash_built_in_parts_is_the_same() {
        let args = ["clang++", "-std=c++26", "-c", "a.cpp", "-o", "a.o"].map(OsStr::new);
        let whole = CommandHash::new("clang 23", args);
        let mut shared = CommandHasher::new("clang 23");
        shared.arg("clang++").arg("-std=c++26");
        let parts = shared
            .clone()
            .encoded(b"\0-c")
            .arg("a.cpp")
            .encoded(b"\0-o\0a.o")
            .finish();
        assert_eq!(whole, parts);
        assert_ne!(whole, shared.finish());
    }

    #[test]
    fn prefetched_stats_match_checking_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join(".yam");
        // Enough files for the prefetch to use threads.
        let inputs: Vec<PathBuf> = (0..1500)
            .map(|i| {
                let path = dir.path().join(format!("{i}.h"));
                fs::write(&path, format!("// {i}")).unwrap();
                path
            })
            .collect();
        let output = dir.path().join("out");
        fs::write(&output, "out").unwrap();
        let (mut state, _) = State::load(&state_dir);
        state.outputs.record(&output, command("cc"), &inputs);
        state.save().unwrap();

        // Changed before the build starts: the prefetch sees it.
        fs::write(&inputs[700], "// changed").unwrap();
        set_mtime(&inputs[700], 5);
        let (mut state, _) = State::load(&state_dir);
        state.outputs.prefetch(4);
        assert!(
            state
                .outputs
                .current
                .iter()
                .all(|c| !matches!(c, Current::Unchecked))
        );
        assert!(!state.outputs.is_fresh(&output, command("cc"), &inputs));
        state.outputs.record(&output, command("cc"), &inputs);
        state.save().unwrap();

        let (mut state, _) = State::load(&state_dir);
        state.outputs.prefetch(4);
        assert!(state.outputs.is_fresh(&output, command("cc"), &inputs));
        fs::remove_file(&inputs[3]).unwrap();
        let (mut state, _) = State::load(&state_dir);
        state.outputs.prefetch(4);
        assert!(!state.outputs.is_fresh(&output, command("cc"), &inputs));
    }
}
