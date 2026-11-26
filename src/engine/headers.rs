//! The header record: the project headers each object was last compiled with, read
//! from its depfile, the way ninja keeps `.ninja_deps`.
//!
//! Compiles run with `-MMD`, so only project headers are recorded, not the toolchain's
//! own (libc++ alone is over a thousand per file). Toolchain headers change only with
//! the toolchain, whose path and version are already part of every cache key.
//!
//! The record lives in `target/<Profile>/.yam/headers.json` until the binary build
//! state file (#26) takes it over. Staleness (#24) will use it to decide what to
//! recompile; for now it keeps the scan cache honest when a header changes.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::store::{self, StoreError};
use crate::manifest::slash_path;

/// Bumped whenever the file layout changes.
const FORMAT_VERSION: u32 = 1;

/// Headers by object file, loaded from the last build and updated by this one.
#[derive(Debug, Default)]
pub struct HeaderRecord {
    path: PathBuf,
    /// Object path → headers, each relative to the project root with `/` separators
    /// when inside it, otherwise absolute.
    entries: HashMap<String, Vec<String>>,
    changed: bool,
}

impl HeaderRecord {
    /// Load the record at `path`. A missing file is an empty record; an unreadable,
    /// corrupt or outdated one is also empty, with the reason returned.
    #[must_use]
    pub fn load(path: &Path) -> (Self, Option<StoreError>) {
        let mut record = Self {
            path: path.to_path_buf(),
            ..Self::default()
        };
        match store::load(path, FORMAT_VERSION) {
            Ok(entries) => record.entries = entries.unwrap_or_default(),
            Err(err) => {
                record.changed = true;
                return (record, Some(err));
            }
        }
        (record, None)
    }

    /// The headers `object` was last compiled with; empty if it never was.
    #[must_use]
    pub fn get(&self, object: &Path) -> &[String] {
        self.entries.get(&key(object)).map_or(&[], Vec::as_slice)
    }

    /// Record that `object` was compiled with `headers`.
    pub fn record(&mut self, object: &Path, headers: Vec<String>) {
        let key = key(object);
        if self.entries.get(&key) != Some(&headers) {
            self.entries.insert(key, headers);
            self.changed = true;
        }
    }

    /// Forget objects that no longer exist, then write the record if anything changed,
    /// so a no-op build writes nothing.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be written.
    pub fn save_if_changed(&mut self) -> io::Result<()> {
        let before = self.entries.len();
        self.entries.retain(|object, _| Path::new(object).exists());
        if self.changed || self.entries.len() != before {
            store::save(&self.path, FORMAT_VERSION, &self.entries)?;
            self.changed = false;
        }
        Ok(())
    }
}

fn key(object: &Path) -> String {
    object.to_string_lossy().into_owned()
}

/// A depfile's dependencies as recorded headers: `source` (the file being compiled)
/// is left out, and paths inside `root` become relative with `/` separators.
#[must_use]
pub fn headers_from_depfile(deps: Vec<String>, root: &Path, source: &Path) -> Vec<String> {
    deps.into_iter()
        .filter_map(|dep| {
            let path = Path::new(&dep);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            if absolute == source {
                return None;
            }
            Some(match absolute.strip_prefix(root) {
                Ok(relative) => slash_path(relative),
                Err(_) => dep,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn records_saves_and_forgets_missing_objects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".yam/headers.json");
        let kept = dir.path().join("a.o");
        let gone = dir.path().join("b.o");
        fs::write(&kept, "").unwrap();

        let (mut record, error) = HeaderRecord::load(&path);
        assert!(error.is_none());
        assert_eq!(record.get(&kept), &[] as &[String]);
        record.record(&kept, vec!["src/a.hpp".into()]);
        record.record(&gone, vec!["src/b.hpp".into()]);
        record.save_if_changed().unwrap();

        let (mut record, _) = HeaderRecord::load(&path);
        assert_eq!(record.get(&kept), ["src/a.hpp"]);
        assert_eq!(
            record.get(&gone),
            &[] as &[String],
            "missing objects are dropped"
        );

        // Recording the same headers again changes nothing, so nothing is written.
        record.record(&kept, vec!["src/a.hpp".into()]);
        fs::remove_file(&path).unwrap();
        record.save_if_changed().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_corrupt_record_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headers.json");
        fs::write(&path, "{").unwrap();
        let (record, error) = HeaderRecord::load(&path);
        assert!(matches!(error, Some(StoreError::Invalid { .. })));
        assert_eq!(record.get(Path::new("a.o")), &[] as &[String]);
    }

    #[test]
    fn depfile_paths_become_project_relative() {
        let base = std::env::temp_dir();
        let root = base.join("p");
        let text = |path: PathBuf| path.to_string_lossy().into_owned();
        let elsewhere = text(base.join("elsewhere").join("c.hpp"));
        let deps = vec![
            text(root.join("src").join("a.cpp")),
            text(root.join("src").join("a.hpp")),
            "include/b.hpp".into(),
            elsewhere.clone(),
        ];
        assert_eq!(
            headers_from_depfile(deps, &root, &root.join("src").join("a.cpp")),
            ["src/a.hpp".to_string(), "include/b.hpp".into(), elsewhere]
        );
    }
}
