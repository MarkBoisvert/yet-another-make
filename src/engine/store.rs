//! Small versioned JSON files under `target/<Profile>/.yam/` that carry results from
//! one build to the next (the scan cache, the header record). They're a stopgap: the
//! binary build state file (#26) takes them over.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Why a stored file was discarded.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
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
struct File<T> {
    version: u32,
    data: T,
}

/// Read the file at `path`, written with format `version`. `Ok(None)` if it doesn't
/// exist.
///
/// # Errors
///
/// Fails if the file can't be read, isn't valid, or has another version.
pub fn load<T: DeserializeOwned>(path: &Path, version: u32) -> Result<Option<T>, StoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StoreError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_slice::<File<T>>(&bytes)
        .ok()
        .filter(|file| file.version == version)
        .map(|file| Some(file.data))
        .ok_or_else(|| StoreError::Invalid {
            path: path.to_path_buf(),
        })
}

/// Write `data` to `path` as format `version`, replacing the file atomically: an
/// interrupted write leaves the old file, never half of a new one.
///
/// # Errors
///
/// Fails if the directory or file can't be written.
pub fn save<T: Serialize>(path: &Path, version: u32, data: &T) -> io::Result<()> {
    let json = serde_json::to_vec(&File { version, data }).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    fs::write(&temp, json)?;
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn round_trips_and_rejects_other_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".yam/thing.json");
        assert!(matches!(load::<BTreeMap<String, u32>>(&path, 1), Ok(None)));

        let data = BTreeMap::from([("a".to_string(), 1)]);
        save(&path, 1, &data).unwrap();
        assert_eq!(load(&path, 1).unwrap(), Some(data));
        assert!(!dir.path().join(".yam/thing.json.tmp").exists());

        let err = load::<BTreeMap<String, u32>>(&path, 2).unwrap_err();
        assert!(matches!(err, StoreError::Invalid { .. }), "{err:?}");
        fs::write(&path, "not json").unwrap();
        let err = load::<BTreeMap<String, u32>>(&path, 1).unwrap_err();
        assert!(matches!(err, StoreError::Invalid { .. }), "{err:?}");
    }
}
