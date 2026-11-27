//! The persistent scan cache: each file's module dependencies from earlier builds,
//! so only new or changed files are scanned, and a no-op build runs no scan at all.
//!
//! A result is keyed on `blake3(file contents)`, the contents of the project headers
//! the file was last compiled with ([`super::HeaderRecord`]), and a hash of everything
//! else that affects scanning: the scanner and compiler (path and version) and the
//! flags the file is scanned with (standard, profile flags, include directories). A
//! header change that adds or removes an import, or flips a macro around one, is a new
//! key. The key doesn't include the file's own path, so identical files share an
//! entry.
//!
//! The cache is part of the build state file ([`super::State`]). Each build keeps
//! only the entries it used, so results for deleted or changed files don't pile up.

use std::collections::HashMap;
use std::ffi::OsStr;

use super::scan::Deps;
use super::state::Record;

/// A header's path and contents hash, or `None` if it can't be read.
pub type HeaderHash<'a> = (&'a str, Option<blake3::Hash>);

/// A cache key: a hash of the flags, the file's contents and its headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub(super) [u8; 32]);

impl Key {
    /// The key for a file whose contents hash to `contents`, scanned with flags
    /// hashed as `flags`, and last compiled with `headers`.
    #[must_use]
    pub fn new(flags: &FlagsHash, contents: &blake3::Hash, headers: &[HeaderHash<'_>]) -> Self {
        // Gathered into one buffer and hashed once: a file can have hundreds of
        // headers, and many tiny `update` calls cost more than the hashing.
        let mut bytes = Vec::with_capacity(64 + headers.len() * 64);
        bytes.extend_from_slice(flags.0.as_bytes());
        bytes.extend_from_slice(contents.as_bytes());
        for (path, hash) in headers {
            bytes.push(0);
            bytes.extend_from_slice(path.as_bytes());
            match hash {
                Some(hash) => {
                    bytes.push(b'=');
                    bytes.extend_from_slice(hash.as_bytes());
                }
                None => bytes.push(b'!'),
            }
        }
        Self(*blake3::hash(&bytes).as_bytes())
    }
}

/// A hash of everything but the file's contents that affects its scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagsHash(blake3::Hash);

impl FlagsHash {
    /// Hash the scanner's identity and `arguments`, the scan command without the
    /// source and output paths (which don't change the result).
    #[must_use]
    pub fn new<'a>(scanner: &str, arguments: impl IntoIterator<Item = &'a OsStr>) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"yam-scan\0");
        hasher.update(scanner.as_bytes());
        for argument in arguments {
            hasher.update(b"\0");
            hasher.update(argument.as_encoded_bytes());
        }
        Self(hasher.finalize())
    }
}

/// Scan results from earlier builds, plus the ones this build used.
#[derive(Debug, Default)]
pub struct ScanCache {
    old: HashMap<Key, Deps>,
    used: HashMap<Key, Deps>,
    /// Whether there's anything to write: a new result, or a state that couldn't be
    /// used as it was.
    changed: bool,
    /// Records for the journal since it was last written.
    pending: Vec<Record>,
}

impl ScanCache {
    pub(super) fn from_entries(entries: HashMap<Key, Deps>) -> Self {
        Self {
            old: entries,
            ..Self::default()
        }
    }

    /// The entries this build used: what the next build keeps.
    pub(super) fn entries(&self) -> &HashMap<Key, Deps> {
        &self.used
    }

    /// Whether the entries to keep differ from those loaded: some were added, or
    /// some went unused and are dropped.
    pub(super) fn needs_save(&self) -> bool {
        self.changed || !self.old.is_empty()
    }

    pub(super) fn saved(&mut self) {
        self.old.clear();
        self.changed = false;
    }

    pub(super) fn mark_changed(&mut self) {
        self.changed = true;
    }

    pub(super) fn take_pending(&mut self) -> Vec<Record> {
        std::mem::take(&mut self.pending)
    }

    /// The cached result for `key`, keeping it for the next build.
    pub fn get(&mut self, key: Key) -> Option<Deps> {
        if let Some(deps) = self.used.get(&key) {
            return Some(deps.clone());
        }
        let deps = self.old.remove(&key)?;
        self.used.insert(key, deps.clone());
        Some(deps)
    }

    /// Record a scan result, unless the same one is already there.
    pub fn insert(&mut self, key: Key, deps: Deps) {
        if self.get(key).as_ref() != Some(&deps) {
            self.pending.push(Record::Scan {
                key,
                deps: deps.clone(),
            });
            self.used.insert(key, deps);
            self.changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan::Provided;
    use crate::engine::state::State;
    use std::fs;

    fn deps(name: &str) -> Deps {
        Deps {
            provides: Some(Provided {
                name: name.into(),
                is_interface: true,
            }),
            requires: vec!["std".into()],
        }
    }

    fn flags(args: &[&str]) -> FlagsHash {
        FlagsHash::new("clang-scan-deps 23.1.6", args.iter().map(OsStr::new))
    }

    fn key(contents: &[u8]) -> Key {
        Key::new(&flags(&[]), &blake3::hash(contents), &[])
    }

    #[test]
    fn keys_change_with_contents_flags_and_headers() {
        let base = flags(&["clang++", "-std=c++26"]);
        let contents = blake3::hash(b"export module a;");
        let key = Key::new(&base, &contents, &[]);
        assert_eq!(key, Key::new(&base, &contents, &[]));
        assert_ne!(
            key,
            Key::new(&base, &blake3::hash(b"export module b;"), &[])
        );
        assert_ne!(
            key,
            Key::new(&flags(&["clang++", "-std=c++23"]), &contents, &[])
        );
        // Argument boundaries count: `-Ia` + `b` isn't `-Iab`.
        assert_ne!(flags(&["-Ia", "b"]), flags(&["-Iab"]));
        assert_ne!(
            FlagsHash::new("clang-scan-deps 22.1.8", [OsStr::new("clang++")]),
            FlagsHash::new("clang-scan-deps 23.1.6", [OsStr::new("clang++")])
        );

        let (v1, v2) = (blake3::hash(b"#define X 0"), blake3::hash(b"#define X 1"));
        let with = |headers: &[HeaderHash<'_>]| Key::new(&base, &contents, headers);
        assert_ne!(key, with(&[("src/x.h", Some(v1))]));
        assert_ne!(
            with(&[("src/x.h", Some(v1))]),
            with(&[("src/x.h", Some(v2))])
        );
        assert_ne!(
            with(&[("src/x.h", Some(v1))]),
            with(&[("src/y.h", Some(v1))])
        );
        assert_ne!(with(&[("src/x.h", Some(v1))]), with(&[("src/x.h", None)]));
    }

    fn load(dir: &std::path::Path) -> State {
        let (state, errors) = State::load(dir);
        assert!(errors.is_empty(), "{errors:?}");
        state
    }

    #[test]
    fn saves_and_loads_only_the_entries_a_build_used() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (key(b"a"), key(b"b"));

        let mut state = load(dir.path());
        assert_eq!(state.scan.get(a), None);
        state.scan.insert(a, deps("a"));
        state.scan.insert(b, deps("b"));
        state.save().unwrap();

        // The next build uses only `a`, so `b` is dropped when it saves.
        let mut state = load(dir.path());
        assert_eq!(state.scan.get(a), Some(deps("a")));
        assert_eq!(state.scan.get(a), Some(deps("a")));
        state.save().unwrap();

        let mut state = load(dir.path());
        assert_eq!(state.scan.get(a), Some(deps("a")));
        assert_eq!(state.scan.get(b), None);
    }

    #[test]
    fn a_build_that_used_exactly_the_cache_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state");
        let a = key(b"a");
        let mut state = load(dir.path());
        state.save().unwrap();
        assert!(!file.exists());
        state.scan.insert(a, deps("a"));
        state.save().unwrap();
        assert!(file.exists());

        let mut state = load(dir.path());
        assert_eq!(state.scan.get(a), Some(deps("a")));
        // Re-recording the same result isn't a change either.
        state.scan.insert(a, deps("a"));
        fs::remove_file(&file).unwrap();
        state.save().unwrap();
        assert!(!file.exists(), "an unchanged cache was rewritten");
    }
}
