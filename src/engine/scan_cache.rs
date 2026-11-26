//! The persistent scan cache: each file's module dependencies from earlier builds,
//! so only new or changed files are scanned, and a no-op build runs no scan at all.
//!
//! A result is keyed on `blake3(file contents)` plus a hash of everything else that
//! affects scanning: the scanner and compiler (path and version) and the flags the
//! file is scanned with (standard, profile flags, include directories). The key
//! doesn't include the file's path, so identical files share an entry. Headers aren't
//! part of the key: a header change that adds or removes an import isn't seen until
//! header dependencies (#23) are tracked.
//!
//! The cache lives in `target/<Profile>/.yam/scan-cache.json` until the binary build
//! state file (#26) takes it over. Each build writes back only the entries it used, so
//! results for deleted or changed files don't pile up. An unreadable or outdated
//! cache is discarded with a note, and every file is scanned again.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::scan::Deps;

/// Bumped whenever the file layout or the meaning of a key changes.
const FORMAT_VERSION: u32 = 1;

/// A cache key: `blake3(flags hash ‖ content hash)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key([u8; 32]);

impl Key {
    /// The key for a file with `contents`, scanned with flags hashed as `flags`.
    #[must_use]
    pub fn new(flags: &FlagsHash, contents: &[u8]) -> Self {
        let content = blake3::hash(contents);
        let mut hasher = blake3::Hasher::new();
        hasher.update(flags.0.as_bytes());
        hasher.update(content.as_bytes());
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

/// Why a cache file was discarded.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("couldn't read `{}`", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{}` is corrupt or from another yam version", path.display())]
    Invalid { path: PathBuf },
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    /// Keys as hex.
    entries: HashMap<String, Deps>,
}

impl ScanCache {
    /// Load the cache at `path`. A missing file is an empty cache. An unreadable,
    /// corrupt or outdated one is also empty, with the reason returned so the caller
    /// can say why every file is being scanned.
    #[must_use]
    pub fn load(path: &Path) -> (Self, Option<CacheError>) {
        let mut cache = Self {
            path: path.to_path_buf(),
            ..Self::default()
        };
        let text = match fs::read(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return (cache, None),
            Err(source) => {
                cache.changed = true;
                let error = CacheError::Read {
                    path: path.to_path_buf(),
                    source,
                };
                return (cache, Some(error));
            }
        };
        let file = serde_json::from_slice::<File>(&text)
            .ok()
            .filter(|file| file.version == FORMAT_VERSION);
        let Some(file) = file else {
            cache.changed = true;
            let error = CacheError::Invalid {
                path: path.to_path_buf(),
            };
            return (cache, Some(error));
        };
        cache.old = file
            .entries
            .into_iter()
            .filter_map(|(hex, deps)| Some((Key::from_hex(&hex)?, deps)))
            .collect();
        (cache, None)
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

    /// Record a new scan result.
    pub fn insert(&mut self, key: Key, deps: Deps) {
        self.used.insert(key, deps);
        self.changed = true;
    }

    /// [`Self::save`], unless this build used exactly what was loaded, so a no-op
    /// build writes nothing.
    ///
    /// # Errors
    ///
    /// Fails if the directory or file can't be written.
    pub fn save_if_changed(&self) -> io::Result<()> {
        if self.changed || !self.old.is_empty() {
            self.save()
        } else {
            Ok(())
        }
    }

    /// Write the entries this build used, replacing the file atomically: an
    /// interrupted write leaves the old cache, never half of a new one.
    ///
    /// # Errors
    ///
    /// Fails if the directory or file can't be written.
    pub fn save(&self) -> io::Result<()> {
        let file = File {
            version: FORMAT_VERSION,
            entries: self
                .used
                .iter()
                .map(|(key, deps)| (key.to_hex(), deps.clone()))
                .collect(),
        };
        let json = serde_json::to_vec(&file).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, json)?;
        fs::rename(&temp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan::Provided;

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

    #[test]
    fn keys_change_with_contents_and_flags() {
        let base = flags(&["clang++", "-std=c++26"]);
        let key = Key::new(&base, b"export module a;");
        assert_eq!(key, Key::new(&base, b"export module a;"));
        assert_ne!(key, Key::new(&base, b"export module b;"));
        assert_ne!(
            key,
            Key::new(&flags(&["clang++", "-std=c++23"]), b"export module a;")
        );
        // Argument boundaries count: `-I a` + `b` isn't `-I ab`.
        assert_ne!(flags(&["-Ia", "b"]), flags(&["-Iab"]));
        assert_ne!(
            FlagsHash::new("clang-scan-deps 22.1.8", [OsStr::new("clang++")]),
            FlagsHash::new("clang-scan-deps 23.1.6", [OsStr::new("clang++")])
        );
    }

    #[test]
    fn saves_and_loads_only_the_entries_a_build_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".yam/scan-cache.json");
        let flags = flags(&["clang++"]);
        let (a, b) = (Key::new(&flags, b"a"), Key::new(&flags, b"b"));

        let (mut cache, error) = ScanCache::load(&path);
        assert!(error.is_none());
        assert_eq!(cache.get(a), None);
        cache.insert(a, deps("a"));
        cache.insert(b, deps("b"));
        cache.save().unwrap();

        // The next build uses only `a`, so `b` is dropped when it saves.
        let (mut cache, error) = ScanCache::load(&path);
        assert!(error.is_none());
        assert_eq!(cache.get(a), Some(deps("a")));
        assert_eq!(cache.get(a), Some(deps("a")));
        cache.save().unwrap();

        let (mut cache, _) = ScanCache::load(&path);
        assert_eq!(cache.get(a), Some(deps("a")));
        assert_eq!(cache.get(b), None);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_build_that_used_exactly_the_cache_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan-cache.json");
        let key = Key::new(&flags(&[]), b"a");
        let (mut cache, _) = ScanCache::load(&path);
        cache.save_if_changed().unwrap();
        assert!(!path.exists());
        cache.insert(key, deps("a"));
        cache.save_if_changed().unwrap();
        assert!(path.exists());

        let (mut cache, _) = ScanCache::load(&path);
        assert_eq!(cache.get(key), Some(deps("a")));
        fs::remove_file(&path).unwrap();
        cache.save_if_changed().unwrap();
        assert!(!path.exists(), "an unchanged cache was rewritten");
    }

    #[test]
    fn corrupt_or_outdated_caches_are_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan-cache.json");
        for text in ["not json", r#"{"version": 999, "entries": {}}"#] {
            fs::write(&path, text).unwrap();
            let (mut cache, error) = ScanCache::load(&path);
            assert!(
                matches!(error, Some(CacheError::Invalid { .. })),
                "{error:?}"
            );
            assert_eq!(cache.get(Key::new(&flags(&[]), b"x")), None);
        }
    }
}
