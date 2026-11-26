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
//! The record lives in `target/<Profile>/.yam/outputs.json` until the binary build
//! state file (#26) takes it over. Paths and hashes are interned, so each input of
//! each output is two small numbers.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::store::{self, StoreError};

/// Bumped whenever the file layout changes.
const FORMAT_VERSION: u32 = 1;

/// A hash of an output's whole command, including the tools that run it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandHash([u8; 32]);

impl CommandHash {
    /// Hash `tools` (the compiler's path and version, say) and the command's
    /// arguments, program first.
    #[must_use]
    pub fn new<'a>(tools: &str, arguments: impl IntoIterator<Item = &'a OsStr>) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"yam-command\0");
        hasher.update(tools.as_bytes());
        for argument in arguments {
            hasher.update(b"\0");
            hasher.update(argument.as_encoded_bytes());
        }
        Self(*hasher.finalize().as_bytes())
    }
}

/// A file's mtime (nanoseconds since the epoch) and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stat {
    mtime: u64,
    size: u64,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    /// The command hash, as hex.
    command: String,
    /// `(path id, hash id)` per input.
    inputs: Vec<(u32, u32)>,
}

/// What yam last saw of a file: its stat and contents hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Seen {
    stat: Stat,
    hash: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Data {
    paths: Vec<String>,
    /// Contents hashes as hex.
    hashes: Vec<String>,
    /// By path id.
    seen: HashMap<u32, Seen>,
    /// By output path.
    outputs: HashMap<String, Entry>,
}

/// The output record, loaded from the last build and updated by this one.
#[derive(Default)]
pub struct Outputs {
    path: PathBuf,
    data: Data,
    path_ids: HashMap<String, u32>,
    hash_ids: HashMap<String, u32>,
    /// Each file's contents hash id as of this build (`None` if it can't be read),
    /// so each file is checked once.
    current: HashMap<u32, Option<u32>>,
    changed: bool,
}

impl Outputs {
    /// Load the record at `path`. A missing file is an empty record; an unreadable,
    /// corrupt or outdated one is also empty, with the reason returned.
    #[must_use]
    pub fn load(path: &Path) -> (Self, Option<StoreError>) {
        let mut outputs = Self {
            path: path.to_path_buf(),
            ..Self::default()
        };
        let error = match store::load::<Data>(path, FORMAT_VERSION) {
            Ok(data) => {
                outputs.data = data.unwrap_or_default();
                None
            }
            Err(err) => {
                outputs.changed = true;
                Some(err)
            }
        };
        outputs.path_ids = index(&outputs.data.paths);
        outputs.hash_ids = index(&outputs.data.hashes);
        (outputs, error)
    }

    /// Whether `output` exists and was built by `command` from `inputs` as they are
    /// now. Every input is checked (and remembered for [`Self::record`]), even after
    /// one is found changed, so a rebuild records the inputs as they were before it
    /// ran.
    pub fn is_fresh(&mut self, output: &Path, command: CommandHash, inputs: &[PathBuf]) -> bool {
        let current: Vec<(u32, Option<u32>)> = inputs
            .iter()
            .map(|input| {
                let id = self.path_id(input);
                (id, self.current_hash(id))
            })
            .collect();
        let Some(entry) = self.data.outputs.get(&key(output)) else {
            return false;
        };
        entry.command == hex(&command.0)
            && entry.inputs.len() == current.len()
            && entry
                .inputs
                .iter()
                .zip(&current)
                .all(|(&(path, hash), &(now_path, now_hash))| {
                    path == now_path && Some(hash) == now_hash
                })
            && output.exists()
    }

    /// Record that `output` was built by `command` from `inputs`. Inputs already
    /// checked this build keep the contents seen then; an input that can't be read
    /// isn't recorded, so the output stays stale until it can be.
    pub fn record(&mut self, output: &Path, command: CommandHash, inputs: &[PathBuf]) {
        let mut recorded = Vec::with_capacity(inputs.len());
        let mut complete = true;
        for input in inputs {
            let id = self.path_id(input);
            match self.current_hash(id) {
                Some(hash) => recorded.push((id, hash)),
                None => complete = false,
            }
        }
        let entry = Entry {
            command: hex(&command.0),
            inputs: recorded,
        };
        let key = key(output);
        if !complete {
            self.changed |= self.data.outputs.remove(&key).is_some();
        } else if self.data.outputs.get(&key) != Some(&entry) {
            self.data.outputs.insert(key, entry);
            self.changed = true;
        }
    }

    /// Forget what this build knows of `path`, because a step just wrote it.
    pub fn invalidate(&mut self, path: &Path) {
        if let Some(&id) = self.path_ids.get(&key(path)) {
            self.current.remove(&id);
        }
    }

    /// Write the record if anything changed, so a no-op build writes nothing.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be written.
    pub fn save_if_changed(&mut self) -> io::Result<()> {
        if self.changed {
            store::save(&self.path, FORMAT_VERSION, &self.data)?;
            self.changed = false;
        }
        Ok(())
    }

    fn path_id(&mut self, path: &Path) -> u32 {
        let key = key(path);
        if let Some(&id) = self.path_ids.get(&key) {
            return id;
        }
        let id = u32::try_from(self.data.paths.len()).unwrap_or(u32::MAX);
        self.data.paths.push(key.clone());
        self.path_ids.insert(key, id);
        self.changed = true;
        id
    }

    fn hash_id(&mut self, hash: &blake3::Hash) -> u32 {
        let hex = hash.to_hex().to_string();
        if let Some(&id) = self.hash_ids.get(&hex) {
            return id;
        }
        let id = u32::try_from(self.data.hashes.len()).unwrap_or(u32::MAX);
        self.data.hashes.push(hex.clone());
        self.hash_ids.insert(hex, id);
        self.changed = true;
        id
    }

    /// The file's contents hash id now: reused from the last time yam saw it when
    /// its stat is unchanged, otherwise read and hashed.
    fn current_hash(&mut self, id: u32) -> Option<u32> {
        if let Some(&hash) = self.current.get(&id) {
            return hash;
        }
        let path = PathBuf::from(&self.data.paths[id as usize]);
        let hash = Stat::of(&path).and_then(|stat| match self.data.seen.get(&id) {
            Some(seen) if seen.stat == stat => Some(seen.hash),
            _ => {
                let bytes = fs::read(&path).ok()?;
                let hash = self.hash_id(&blake3::hash(&bytes));
                self.data.seen.insert(id, Seen { stat, hash });
                self.changed = true;
                Some(hash)
            }
        });
        self.current.insert(id, hash);
        hash
    }
}

fn index(items: &[String]) -> HashMap<String, u32> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.clone(), u32::try_from(i).unwrap_or(u32::MAX)))
        .collect()
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn hex(bytes: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        _dir: tempfile::TempDir,
        record: PathBuf,
        source: PathBuf,
        header: PathBuf,
        object: PathBuf,
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
            record: path(".yam/outputs.json"),
            source: path("a.cpp"),
            header: path("a.h"),
            object: path("a.o"),
            _dir: dir,
        }
    }

    /// Build once, then load the saved record as the next build would.
    fn built(p: &Project) -> Outputs {
        let inputs = [p.source.clone(), p.header.clone()];
        let (mut outputs, error) = Outputs::load(&p.record);
        assert!(error.is_none());
        assert!(!outputs.is_fresh(&p.object, command("cc"), &inputs));
        outputs.record(&p.object, command("cc"), &inputs);
        outputs.save_if_changed().unwrap();
        Outputs::load(&p.record).0
    }

    #[test]
    fn an_unchanged_output_is_fresh_and_writes_nothing() {
        let p = project();
        let mut outputs = built(&p);
        let inputs = [p.source.clone(), p.header.clone()];
        assert!(outputs.is_fresh(&p.object, command("cc"), &inputs));
        fs::remove_file(&p.record).unwrap();
        outputs.save_if_changed().unwrap();
        assert!(!p.record.exists(), "a no-op build rewrote the record");
    }

    #[test]
    fn a_new_mtime_with_the_same_contents_is_fresh() {
        let p = project();
        let mut outputs = built(&p);
        set_mtime(&p.header, 2_000);
        let inputs = [p.source.clone(), p.header.clone()];
        assert!(outputs.is_fresh(&p.object, command("cc"), &inputs));
    }

    #[test]
    fn changed_contents_commands_inputs_or_outputs_are_stale() {
        let p = project();
        let inputs = [p.source.clone(), p.header.clone()];

        let mut outputs = built(&p);
        fs::write(&p.header, "#pragma once\nint x;").unwrap();
        assert!(!outputs.is_fresh(&p.object, command("cc"), &inputs));

        let p = project();
        let mut outputs = built(&p);
        assert!(!outputs.is_fresh(&p.object, command("cc -O2"), &inputs));
        assert!(!outputs.is_fresh(&p.object, command("cc"), &inputs[..1]));

        let p = project();
        let mut outputs = built(&p);
        fs::remove_file(&p.object).unwrap();
        assert!(!outputs.is_fresh(&p.object, command("cc"), &inputs));

        let p = project();
        let mut outputs = built(&p);
        fs::remove_file(&p.header).unwrap();
        assert!(!outputs.is_fresh(&p.object, command("cc"), &inputs));
    }

    #[test]
    fn a_rebuild_records_inputs_as_they_were_when_checked() {
        let p = project();
        let mut outputs = built(&p);
        let inputs = [p.source.clone(), p.header.clone()];
        fs::write(&p.source, "int a(); // edited").unwrap();
        assert!(!outputs.is_fresh(&p.object, command("cc"), &inputs));
        // The source changes again while the compile runs, after it was read.
        fs::write(&p.source, "int a(); // edited twice").unwrap();
        outputs.record(&p.object, command("cc"), &inputs);
        outputs.save_if_changed().unwrap();

        let (mut next, _) = Outputs::load(&p.record);
        assert!(
            !next.is_fresh(&p.object, command("cc"), &inputs),
            "the second edit wasn't compiled, so the next build must rebuild"
        );
    }

    #[test]
    fn an_output_written_this_build_is_checked_again() {
        let p = project();
        let mut outputs = built(&p);
        let inputs = [p.source.clone(), p.header.clone()];
        assert!(outputs.is_fresh(&p.object, command("cc"), &inputs));
        fs::write(&p.header, "#pragma once\n// rewritten by a step").unwrap();
        outputs.invalidate(&p.header);
        assert!(!outputs.is_fresh(&p.object, command("cc"), &inputs));
    }

    #[test]
    fn a_corrupt_record_is_discarded() {
        let p = project();
        fs::create_dir_all(p.record.parent().unwrap()).unwrap();
        fs::write(&p.record, "[").unwrap();
        let (mut outputs, error) = Outputs::load(&p.record);
        assert!(matches!(error, Some(StoreError::Invalid { .. })));
        assert!(!outputs.is_fresh(&p.object, command("cc"), std::slice::from_ref(&p.source)));
    }
}
