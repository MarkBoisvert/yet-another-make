//! Glob-based source selection, relative to a project root.
//!
//! Globs use `/` separators, `*` doesn't cross directories, `**` does, and `\` escapes a
//! metacharacter on every platform. Everything stays inside the project root.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::ManifestError;

/// The build directory, skipped when scanning a project.
const TARGET_DIR: &str = "target";

/// Every file under a project root, as sorted `/`-separated relative paths. Skips the
/// `target/` build directory and hidden entries (`.git`, `.cache`, …).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceTree {
    files: Vec<String>,
}

/// A directory as the last walk saw it: its mtime (nanoseconds since the epoch) and
/// its entries, each a name and whether it's a directory. Hidden entries and the
/// root's `target/` are already left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub mtime: u64,
    pub entries: Vec<(String, bool)>,
}

/// Directory listings from the last walk, so an unchanged directory isn't read again:
/// a directory's mtime changes whenever an entry is added, removed or renamed. Kept in
/// the build state file.
#[derive(Debug, Default)]
pub struct DirCache {
    old: HashMap<String, Listing>,
    used: HashMap<String, Listing>,
    changed: bool,
}

/// A listing this young isn't trusted next time: an entry added within the same
/// mtime tick wouldn't change the directory's mtime.
const RACY: Duration = Duration::from_secs(2);

impl DirCache {
    /// A cache from the listings the state file holds, by `/`-separated directory
    /// path relative to the root (`""` for the root itself).
    #[must_use]
    pub fn from_listings(listings: HashMap<String, Listing>) -> Self {
        Self {
            old: listings,
            ..Self::default()
        }
    }

    /// The listings this walk used: what the next build keeps.
    #[must_use]
    pub fn listings(&self) -> &HashMap<String, Listing> {
        &self.used
    }

    /// Whether the listings to keep differ from those loaded.
    #[must_use]
    pub fn needs_save(&self) -> bool {
        self.changed || !self.old.is_empty()
    }

    pub fn saved(&mut self) {
        self.old.clear();
        self.changed = false;
    }

    pub fn mark_changed(&mut self) {
        self.changed = true;
    }

    /// The entries of `dir` (at `relative`), reused when its mtime hasn't moved.
    fn list(
        &mut self,
        dir: &Path,
        relative: &str,
        depth: usize,
    ) -> Result<Vec<(String, bool)>, ManifestError> {
        let list_err = |source| ManifestError::ListDir {
            path: dir.to_path_buf(),
            source,
        };
        let modified = fs::metadata(dir)
            .and_then(|meta| meta.modified())
            .map_err(list_err)?;
        let mtime = nanos(modified);
        let old = self.old.remove(relative);
        if let Some(listing) = &old
            && listing.mtime == mtime
            && mtime != 0
        {
            let entries = listing.entries.clone();
            self.used.insert(relative.to_string(), listing.clone());
            return Ok(entries);
        }

        let mut entries = Vec::new();
        for entry in fs::read_dir(dir).map_err(list_err)? {
            let entry = entry.map_err(list_err)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || (depth == 0 && name == TARGET_DIR) {
                continue;
            }
            // Symlinks aren't followed, as before: only real files and directories.
            let kind = entry.file_type().map_err(list_err)?;
            if kind.is_dir() || kind.is_file() {
                entries.push((name, kind.is_dir()));
            }
        }
        entries.sort();
        let young = SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age < RACY);
        let listing = Listing {
            mtime: if young { 0 } else { mtime },
            entries: entries.clone(),
        };
        // Reading a directory again isn't a change unless what it holds is.
        self.changed |= old.as_ref() != Some(&listing);
        self.used.insert(relative.to_string(), listing);
        Ok(entries)
    }
}

fn nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
    })
}

impl SourceTree {
    /// Walk the project root once.
    ///
    /// # Errors
    ///
    /// Fails if a directory under `root` can't be read.
    pub fn scan(root: &Path) -> Result<Self, ManifestError> {
        Self::scan_cached(root, &mut DirCache::default())
    }

    /// Walk the project root, reusing the listings in `cache` for directories that
    /// haven't changed, and recording the rest.
    ///
    /// # Errors
    ///
    /// Fails if a directory under `root` can't be read.
    pub fn scan_cached(root: &Path, cache: &mut DirCache) -> Result<Self, ManifestError> {
        let mut files = Vec::new();
        let mut pending = vec![(String::new(), 0usize)];
        while let Some((relative, depth)) = pending.pop() {
            let dir = if relative.is_empty() {
                root.to_path_buf()
            } else {
                root.join(&relative)
            };
            for (name, is_dir) in cache.list(&dir, &relative, depth)? {
                let child = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                if is_dir {
                    pending.push((child, depth + 1));
                } else {
                    files.push(child);
                }
            }
        }
        files.sort();
        Ok(Self { files })
    }

    /// All files, sorted.
    #[must_use]
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// Files matching any `include` glob and no `exclude` glob, sorted.
    #[must_use]
    pub fn select(&self, include: &GlobList, exclude: &GlobList) -> Vec<&str> {
        self.files
            .iter()
            .map(String::as_str)
            .filter(|file| include.is_match(file) && !exclude.is_match(file))
            .collect()
    }

    /// Indices of the globs in `globs` that match no file at all.
    #[must_use]
    pub fn unmatched(&self, globs: &GlobList) -> Vec<usize> {
        let mut matched = HashSet::new();
        for file in &self.files {
            matched.extend(globs.set.matches(file.as_str()));
        }
        (0..globs.len).filter(|i| !matched.contains(i)).collect()
    }
}

/// A compiled list of globs.
#[derive(Clone, Debug)]
pub struct GlobList {
    set: GlobSet,
    len: usize,
}

/// A glob that couldn't be compiled, with its index in the list.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid glob '{pattern}': {reason}")]
pub struct GlobError {
    pub index: usize,
    pub pattern: String,
    pub reason: String,
}

impl GlobList {
    /// Compile `patterns`.
    ///
    /// # Errors
    ///
    /// Fails on the first pattern that is malformed or points outside the project
    /// root (absolute, or containing a `..` segment).
    pub fn new(patterns: &[String]) -> Result<Self, GlobError> {
        let mut builder = GlobSetBuilder::new();
        for (index, pattern) in patterns.iter().enumerate() {
            let error = |reason: String| GlobError {
                index,
                pattern: pattern.clone(),
                reason,
            };
            if !stays_inside_root(Path::new(pattern)) {
                return Err(error("must be relative and inside the project root".into()));
            }
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .map_err(|err| error(err.kind().to_string()))?;
            builder.add(glob);
        }
        let set = builder.build().map_err(|err| GlobError {
            index: 0,
            pattern: String::new(),
            reason: err.to_string(),
        })?;
        Ok(Self {
            set,
            len: patterns.len(),
        })
    }

    /// Whether any glob matches the `/`-separated relative `path`.
    #[must_use]
    pub fn is_match(&self, path: &str) -> bool {
        self.set.is_match(path)
    }
}

/// Whether `path` is relative and never climbs out of the directory it's joined to.
#[must_use]
pub fn stays_inside_root(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// A relative path with `/` separators.
#[must_use]
pub fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(files: &[&str]) -> (tempfile::TempDir, SourceTree) {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let tree = SourceTree::scan(dir.path()).unwrap();
        (dir, tree)
    }

    fn globs(patterns: &[&str]) -> GlobList {
        GlobList::new(
            &patterns
                .iter()
                .map(|p| (*p).to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn scan_skips_target_and_hidden_entries() {
        let (_dir, tree) = tree(&[
            "src/main.cpp",
            "src/a/b.cppm",
            "target/Debug/x.o",
            ".git/config",
            "src/.hidden.cpp",
            "lib/target/kept.cpp",
        ]);
        assert_eq!(
            tree.files(),
            ["lib/target/kept.cpp", "src/a/b.cppm", "src/main.cpp"]
        );
    }

    #[test]
    fn star_stays_in_one_directory_and_double_star_recurses() {
        let (_dir, tree) = tree(&["src/a.cpp", "src/sub/b.cpp", "src/sub/deep/c.cpp"]);
        assert_eq!(
            tree.select(&globs(&["src/*.cpp"]), &globs(&[])),
            ["src/a.cpp"]
        );
        assert_eq!(
            tree.select(&globs(&["src/**/*.cpp"]), &globs(&[])),
            ["src/a.cpp", "src/sub/b.cpp", "src/sub/deep/c.cpp"]
        );
    }

    #[test]
    fn excludes_win() {
        let (_dir, tree) = tree(&["src/main.cpp", "src/lib.cpp", "src/bin/tool.cpp"]);
        assert_eq!(
            tree.select(
                &globs(&["src/**/*.cpp"]),
                &globs(&["src/main.cpp", "src/bin/**"])
            ),
            ["src/lib.cpp"]
        );
    }

    #[test]
    fn reports_globs_that_match_nothing() {
        let (_dir, tree) = tree(&["src/a.cpp"]);
        assert_eq!(
            tree.unmatched(&globs(&["src/*.cpp", "lib/**/*.cpp", "src/*.cppm"])),
            [1, 2]
        );
    }

    #[test]
    fn rejects_globs_outside_the_root() {
        for pattern in ["../shared/*.cpp", "/abs/*.cpp", "src/../../x.cpp"] {
            let err = GlobList::new(&[pattern.to_string()]).unwrap_err();
            assert!(
                err.reason.contains("inside the project root"),
                "{pattern}: {err}"
            );
        }
    }

    #[test]
    fn rejects_malformed_globs_with_their_index() {
        let err = GlobList::new(&["src/*.cpp".into(), "src/[a.cpp".into()]).unwrap_err();
        assert_eq!(err.index, 1);
        assert_eq!(err.pattern, "src/[a.cpp");
    }

    #[test]
    fn backslash_escapes_metacharacters() {
        let (_dir, tree) = tree(&["src/odd[1].cpp", "src/odd1.cpp"]);
        assert_eq!(
            tree.select(&globs(&["src/odd\\[1\\].cpp"]), &globs(&[])),
            ["src/odd[1].cpp"]
        );
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// Age every directory under `root` by an hour, as if made long ago.
    #[cfg(unix)]
    fn age(root: &Path) {
        let old = SystemTime::now() - Duration::from_secs(3600);
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_dir() {
                fs::File::open(entry.path())
                    .and_then(|file| file.set_modified(old))
                    .unwrap();
            }
        }
    }

    #[test]
    fn walks_match_with_and_without_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["src/main.cpp", "src/a/b.cppm", "target/x.o", ".git/config"] {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let mut cache = DirCache::default();
        let cached = SourceTree::scan_cached(dir.path(), &mut cache).unwrap();
        assert_eq!(cached.files(), ["src/a/b.cppm", "src/main.cpp"]);
        assert_eq!(cached, SourceTree::scan(dir.path()).unwrap());
        assert!(cache.needs_save());
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_directories_are_reused_and_changes_are_seen() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.cpp"), "").unwrap();
        age(dir.path());

        let mut cache = DirCache::default();
        SourceTree::scan_cached(dir.path(), &mut cache).unwrap();
        cache.saved();
        let mut cache = DirCache::from_listings(cache.listings().clone());
        let tree = SourceTree::scan_cached(dir.path(), &mut cache).unwrap();
        assert_eq!(tree.files(), ["src/a.cpp"]);
        assert!(!cache.needs_save(), "nothing changed, so nothing to write");

        // A new file changes its directory's mtime, so it's listed again.
        fs::write(dir.path().join("src/b.cpp"), "").unwrap();
        let mut cache = DirCache::from_listings(cache.listings().clone());
        let tree = SourceTree::scan_cached(dir.path(), &mut cache).unwrap();
        assert_eq!(tree.files(), ["src/a.cpp", "src/b.cpp"]);
        assert!(cache.needs_save());
    }

    #[test]
    fn a_directory_changed_just_now_isnt_trusted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.cpp"), "").unwrap();
        let mut cache = DirCache::default();
        SourceTree::scan_cached(dir.path(), &mut cache).unwrap();
        assert_eq!(cache.listings()[""].mtime, 0);
    }
}
