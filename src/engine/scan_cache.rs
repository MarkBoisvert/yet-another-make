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
//! The cache lives in `target/<Profile>/.yam/scan-cache.json` until the binary build
//! state file (#26) takes it over. Each build writes back only the entries it used, so
//! results for deleted or changed files don't pile up. An unreadable or outdated
//! cache is discarded with a note, and every file is scanned again.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use super::scan::Deps;
use super::store::{self, StoreError};

/// Bumped whenever the file layout or the meaning of a key changes.
const FORMAT_VERSION: u32 = 2;

/// A header's path and contents hash, or `None` if it can't be read.
pub type HeaderHash<'a> = (&'a str, Option<blake3::Hash>);

/// A cache key: a hash of the flags, the file's contents and its headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key([u8; 32]);

impl Key {
    /// The key for a file whose contents hash to `contents`, scanned with flags
    /// hashed as `flags`, and last compiled with `headers`.
    #[must_use]
    pub fn new(flags: &FlagsHash, contents: &blake3::Hash, headers: &[HeaderHash<'_>]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(flags.0.as_bytes());
        hasher.update(contents.as_bytes());
        for (path, hash) in headers {
            hasher.update(b"\0");
            hasher.update(path.as_bytes());
            match hash {
                Some(hash) => hasher.update(b"=").update(hash.as_bytes()),
                None => hasher.update(b"!"),
            };
        }
        Self(*hasher.finalize().as_bytes())
    }

    fn to_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    fn from_hex(hex: &str) -> Option<Self> {
        blake3::Hash::from_hex(hex)
            .ok()
            .map(|hash| Self(*hash.as_bytes()))
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
    path: PathBuf,
    old: HashMap<Key, Deps>,
    used: HashMap<Key, Deps>,
    /// Whether there's anything to write: a new result, or a file that couldn't be
    /// used as it was.
    changed: bool,
}

impl ScanCache {
    /// Load the cache at `path`. A missing file is an empty cache. An unreadable,
    /// corrupt or outdated one is also empty, with the reason returned so the caller
    /// can say why every file is being scanned.
    #[must_use]
    pub fn load(path: &Path) -> (Self, Option<StoreError>) {
        let mut cache = Self {
            path: path.to_path_buf(),
            ..Self::default()
        };
        match store::load::<HashMap<String, Deps>>(path, FORMAT_VERSION) {
            Ok(entries) => {
                cache.old = entries
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(hex, deps)| Some((Key::from_hex(&hex)?, deps)))
                    .collect();
                (cache, None)
            }
            Err(err) => {
                cache.changed = true;
                (cache, Some(err))
            }
        }
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
            self.used.insert(key, deps);
            self.changed = true;
        }
    }

    /// Write the entries this build used, unless that's exactly what was loaded, so
    /// a no-op build writes nothing.
    ///
    /// # Errors
    ///
    /// Fails if the directory or file can't be written.
    pub fn save_if_changed(&mut self) -> io::Result<()> {
        if !self.changed && self.old.is_empty() {
            return Ok(());
        }
        let entries: HashMap<String, &Deps> = self
            .used
            .iter()
            .map(|(key, deps)| (key.to_hex(), deps))
            .collect();
        store::save(&self.path, FORMAT_VERSION, &entries)?;
        self.old.clear();
        self.changed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan::Provided;
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

    #[test]
    fn saves_and_loads_only_the_entries_a_build_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".yam/scan-cache.json");
        let (a, b) = (key(b"a"), key(b"b"));

        let (mut cache, error) = ScanCache::load(&path);
        assert!(error.is_none());
        assert_eq!(cache.get(a), None);
        cache.insert(a, deps("a"));
        cache.insert(b, deps("b"));
        cache.save_if_changed().unwrap();

        // The next build uses only `a`, so `b` is dropped when it saves.
        let (mut cache, error) = ScanCache::load(&path);
        assert!(error.is_none());
        assert_eq!(cache.get(a), Some(deps("a")));
        assert_eq!(cache.get(a), Some(deps("a")));
        cache.save_if_changed().unwrap();

        let (mut cache, _) = ScanCache::load(&path);
        assert_eq!(cache.get(a), Some(deps("a")));
        assert_eq!(cache.get(b), None);
    }

    #[test]
    fn a_build_that_used_exactly_the_cache_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan-cache.json");
        let a = key(b"a");
        let (mut cache, _) = ScanCache::load(&path);
        cache.save_if_changed().unwrap();
        assert!(!path.exists());
        cache.insert(a, deps("a"));
        cache.save_if_changed().unwrap();
        assert!(path.exists());

        let (mut cache, _) = ScanCache::load(&path);
        assert_eq!(cache.get(a), Some(deps("a")));
        // Re-recording the same result isn't a change either.
        cache.insert(a, deps("a"));
        fs::remove_file(&path).unwrap();
        cache.save_if_changed().unwrap();
        assert!(!path.exists(), "an unchanged cache was rewritten");
    }

    #[test]
    fn corrupt_or_outdated_caches_are_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan-cache.json");
        for text in ["not json", r#"{"version": 999, "data": {}}"#] {
            fs::write(&path, text).unwrap();
            let (mut cache, error) = ScanCache::load(&path);
            assert!(
                matches!(error, Some(StoreError::Invalid { .. })),
                "{error:?}"
            );
            assert_eq!(cache.get(key(b"x")), None);
        }
    }
}
