//! The header record: the project headers each object was last compiled with, read
//! from its depfile, the way ninja keeps `.ninja_deps`.
//!
//! Compiles run with `-MMD`, so only project headers are recorded, not the toolchain's
//! own (libc++ alone is over a thousand per file). Toolchain headers change only with
//! the toolchain, whose path and version are already part of every cache key.
//!
//! The record is part of the build state file ([`super::State`]). Staleness uses it
//! to know each compile's inputs, and the scan cache to notice a header change.

use std::collections::HashMap;
use std::path::Path;

use super::state::Record;
use crate::manifest::slash_path;

/// Headers by object file, loaded from the last build and updated by this one.
#[derive(Debug, Default)]
pub struct HeaderRecord {
    /// Object path → headers, each relative to the project root with `/` separators
    /// when inside it, otherwise absolute.
    entries: HashMap<String, Vec<String>>,
    changed: bool,
    /// Records for the journal since it was last written.
    pending: Vec<Record>,
}

impl HeaderRecord {
    pub(super) fn from_entries(entries: HashMap<String, Vec<String>>) -> Self {
        Self {
            entries,
            ..Self::default()
        }
    }

    pub(super) fn entries(&self) -> &HashMap<String, Vec<String>> {
        &self.entries
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

    /// Forget objects that no longer exist. Done only when the state is written
    /// anyway, so a no-op build doesn't stat every object.
    pub(super) fn prune(&mut self) {
        self.entries.retain(|object, _| Path::new(object).exists());
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
            self.pending.push(Record::Headers {
                object: key.clone(),
                headers: headers.clone(),
            });
            self.entries.insert(key, headers);
            self.changed = true;
        }
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
    use crate::engine::state::State;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn records_saves_and_forgets_missing_objects() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join(".yam");
        let kept = dir.path().join("a.o");
        let gone = dir.path().join("b.o");
        fs::write(&kept, "").unwrap();

        let (mut state, errors) = State::load(&state_dir);
        assert!(errors.is_empty());
        assert_eq!(state.headers.get(&kept), &[] as &[String]);
        state.headers.record(&kept, vec!["src/a.hpp".into()]);
        state.headers.record(&gone, vec!["src/b.hpp".into()]);
        state.save().unwrap();

        let (mut state, _) = State::load(&state_dir);
        assert_eq!(state.headers.get(&kept), ["src/a.hpp"]);
        assert_eq!(
            state.headers.get(&gone),
            &[] as &[String],
            "missing objects are dropped"
        );

        // Recording the same headers again changes nothing, so nothing is written.
        state.headers.record(&kept, vec!["src/a.hpp".into()]);
        fs::remove_file(state_dir.join("state")).unwrap();
        state.save().unwrap();
        assert!(!state_dir.join("state").exists());
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
