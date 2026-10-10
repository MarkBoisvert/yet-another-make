//! Glob-based source selection, relative to a project root.
//!
//! Globs use `/` separators, `*` doesn't cross directories, `**` does, and `\` escapes a
//! metacharacter on every platform. Everything stays inside the project root.

use std::collections::HashSet;
use std::io;
use std::path::{Component, Path};

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

impl SourceTree {
    /// Walk the project root once.
    ///
    /// # Errors
    ///
    /// Fails if a directory under `root` can't be read.
    pub fn scan(root: &Path) -> Result<Self, ManifestError> {
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                entry.depth() == 0
                    || !(name.starts_with('.') || (entry.depth() == 1 && name == TARGET_DIR))
            });

        let mut files = Vec::new();
        for entry in walker {
            let entry = entry.map_err(|err| ManifestError::ListDir {
                path: err.path().unwrap_or(root).to_path_buf(),
                source: io::Error::from(err),
            })?;
            if entry.file_type().is_file()
                && let Ok(relative) = entry.path().strip_prefix(root)
            {
                files.push(slash_path(relative));
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
