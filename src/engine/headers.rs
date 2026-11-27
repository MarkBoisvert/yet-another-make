//! The header record: the project headers each object was last compiled with, read
//! from its depfile, the way ninja keeps `.ninja_deps`.
//!
//! Compiles run with `-MMD`, so only project headers are recorded, not the toolchain's
//! own (libc++ alone is over a thousand per file). Toolchain headers change only with
//! the toolchain, whose path and version are already part of every cache key.
//!
//! The record is part of the build state file ([`super::State`]). Staleness uses it
//! to know each compile's inputs, and the scan cache to notice a header change.

use std::path::Path;

use rustc_hash::FxHashMap;

use super::state::Record;
use super::strings::StrTable;
use crate::manifest::slash_path;

/// Headers by object file, loaded from the last build and updated by this one.
///
/// Header paths are interned: each distinct header is stored once and objects list
/// header ids, so a no-op build compares and hashes small integers rather than the
/// same path strings over and over (#65). Objects are keyed by their id in the
/// output record ([`super::Outputs::intern`]), so finding one compares no paths
/// either.
#[derive(Debug, Default)]
pub struct HeaderRecord {
    /// Each header's path, by id: relative to the project root with `/` separators
    /// when inside it, otherwise absolute.
    names: StrTable,
    /// Object path id → where its headers' ids are in `ids`.
    entries: FxHashMap<u32, (u32, u32)>,
    /// Every object's header ids, back to back, so loading the record allocates
    /// once for all of them. A rerecorded object's ids are appended; the state file
    /// keeps only the live ones.
    ids: Vec<u32>,
    changed: bool,
    /// Records for the journal since it was last written.
    pending: Vec<Record>,
}

impl HeaderRecord {
    /// A record from its tables, as the state file stores them. `None` if a header
    /// id is out of range.
    pub(super) fn from_parts(
        names: StrTable,
        entries: FxHashMap<u32, (u32, u32)>,
        ids: Vec<u32>,
    ) -> Option<Self> {
        let valid = ids.iter().all(|&id| id < names.len())
            && entries
                .values()
                .all(|&(start, len)| (start as usize + len as usize) <= ids.len());
        valid.then(|| Self {
            names,
            entries,
            ids,
            ..Self::default()
        })
    }

    /// The header names, and each object's header ids by object path id.
    pub(super) fn parts(&self) -> (&StrTable, impl Iterator<Item = (u32, &[u32])>) {
        let entries = self
            .entries
            .iter()
            .map(|(&object, &range)| (object, self.slice(range)));
        (&self.names, entries)
    }

    /// How many objects have headers recorded.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    fn slice(&self, (start, len): (u32, u32)) -> &[u32] {
        &self.ids[start as usize..start as usize + len as usize]
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

    /// Forget objects that no longer exist, by `exists(object id)`. Done only when
    /// the state is written anyway, so a no-op build doesn't stat every object.
    /// Header names stay; they're a few bytes each.
    pub(super) fn prune(&mut self, exists: impl Fn(u32) -> bool) {
        self.entries.retain(|&object, _| exists(object));
    }

    /// The ids of the headers object `object` (an output record id) was last
    /// compiled with; empty if it never was.
    #[must_use]
    pub fn get(&self, object: u32) -> &[u32] {
        self.entries
            .get(&object)
            .map_or(&[], |&range| self.slice(range))
    }

    /// A header's path, by id.
    #[must_use]
    pub fn name(&self, id: u32) -> &str {
        self.names.get(id)
    }

    /// The paths of the headers `object` was last compiled with.
    #[must_use]
    pub fn names_of(&self, object: u32) -> Vec<&str> {
        self.get(object).iter().map(|&id| self.name(id)).collect()
    }

    /// Record that object `object`, at `path`, was compiled with `headers`.
    pub fn record(&mut self, object: u32, path: &str, headers: Vec<String>) {
        let ids: Vec<u32> = headers
            .iter()
            .map(|header| self.names.intern(header).0)
            .collect();
        if self.get(object) != ids.as_slice() || !self.entries.contains_key(&object) {
            self.pending.push(Record::Headers {
                object: path.to_string(),
                headers,
            });
            let start = u32::try_from(self.ids.len()).unwrap_or(u32::MAX);
            let len = u32::try_from(ids.len()).unwrap_or(u32::MAX);
            self.ids.extend_from_slice(&ids);
            self.entries.insert(object, (start, len));
            self.changed = true;
        }
    }
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
        let text = |path: &Path| path.to_str().unwrap().to_string();

        let (mut state, errors) = State::load(&state_dir);
        assert!(errors.is_empty());
        let (k, g) = (state.outputs.intern(&kept), state.outputs.intern(&gone));
        assert_eq!(state.headers.get(k), &[] as &[u32]);
        state
            .headers
            .record(k, &text(&kept), vec!["src/a.hpp".into()]);
        state
            .headers
            .record(g, &text(&gone), vec!["src/b.hpp".into()]);
        state.save().unwrap();

        let (mut state, _) = State::load(&state_dir);
        let (k, g) = (state.outputs.intern(&kept), state.outputs.intern(&gone));
        assert_eq!(state.headers.names_of(k), ["src/a.hpp"]);
        assert_eq!(
            state.headers.get(g),
            &[] as &[u32],
            "missing objects are dropped"
        );

        // Recording the same headers again changes nothing, so nothing is written.
        state
            .headers
            .record(k, &text(&kept), vec!["src/a.hpp".into()]);
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
