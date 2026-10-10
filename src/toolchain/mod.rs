//! Locating the Clang toolchain a build uses: `clang++`, `clang-scan-deps`, and
//! libc++'s `std` module sources.
//!
//! `clang++` is looked up in order:
//! 1. the `YAM_CXX` environment variable, a path or a name on `PATH`;
//! 2. the toolchain bundled with yam: `../libexec/yam/` next to the binary, or the
//!    binary's own directory on Windows (see `docs/release.md`);
//! 3. `PATH`: `clang++`, then versioned names such as `clang++-22`.
//!
//! `clang-scan-deps` is looked up the same way (`YAM_CLANG_SCAN_DEPS`), but first next
//! to the chosen `clang++`, so both come from the same install. `clang++` must be
//! Clang 22 or newer, and `clang-scan-deps` must have the same major version.

mod std_modules;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use std_modules::{ModuleSource, StdModules};

/// The oldest Clang major version yam supports.
pub const MIN_CLANG_MAJOR: u32 = 22;

/// Overrides the `clang++` lookup.
pub const CXX_ENV: &str = "YAM_CXX";

/// Overrides the `clang-scan-deps` lookup.
pub const SCAN_DEPS_ENV: &str = "YAM_CLANG_SCAN_DEPS";

/// How many major versions past [`MIN_CLANG_MAJOR`] are tried as versioned names on
/// `PATH` (`clang++-22` … `clang++-30`).
const NEWER_MAJORS: u32 = 8;

/// The tools a build runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolchain {
    pub cxx: Tool,
    pub scan_deps: Tool,
    pub std_modules: StdModules,
}

/// One located executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub path: PathBuf,
    pub version: Version,
}

/// A tool's version, from its `--version` output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// The first `version X.Y.Z` in `--version` output, e.g. `Ubuntu clang version
    /// 22.1.8 (…)` or `LLVM version 22.1.8`. Missing minor or patch numbers are 0.
    #[must_use]
    pub fn parse(output: &str) -> Option<Self> {
        const MARKER: &str = "version ";
        let start = output.find(MARKER)? + MARKER.len();
        let token = output[start..].split_whitespace().next()?;
        let mut parts = token.split('.').map(|part| {
            let digits = part
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(part.len());
            part[..digits].parse::<u32>().ok()
        });
        let major = parts.next().flatten()?;
        let mut next = || parts.next().flatten().unwrap_or(0);
        Some(Self {
            major,
            minor: next(),
            patch: next(),
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Where to look for tools. [`Search::from_env`] reads the real environment; tests
/// build one by hand.
#[derive(Clone, Debug, Default)]
pub struct Search {
    pub cxx_override: Option<OsString>,
    pub scan_deps_override: Option<OsString>,
    pub bundled_dir: Option<PathBuf>,
    pub path: Vec<PathBuf>,
}

impl Search {
    /// The overrides, bundled toolchain directory and `PATH` of this process.
    #[must_use]
    pub fn from_env() -> Self {
        let var = |name| env::var_os(name).filter(|value| !value.is_empty());
        Self {
            cxx_override: var(CXX_ENV),
            scan_deps_override: var(SCAN_DEPS_ENV),
            bundled_dir: env::current_exe()
                .ok()
                .map(|exe| fs::canonicalize(&exe).unwrap_or(exe))
                .and_then(|exe| bundled_dir(&exe)),
            path: var("PATH")
                .map(|path| env::split_paths(&path).collect())
                .unwrap_or_default(),
        }
    }
}

/// The bundled toolchain directory for a `yam` binary at `exe`.
fn bundled_dir(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    Some(if cfg!(windows) {
        dir.to_path_buf()
    } else {
        dir.join("..").join("libexec").join("yam")
    })
}

/// A candidate that was found but not used, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub path: PathBuf,
    pub reason: String,
}

/// Why the toolchain couldn't be located.
#[derive(Debug, thiserror::Error)]
pub enum ToolchainError {
    #[error("{var} is set to `{}`, but {reason}", value.display())]
    Override {
        var: &'static str,
        value: PathBuf,
        reason: String,
    },

    #[error("{}", not_found(tool, wanted, rejected, help))]
    NotFound {
        tool: &'static str,
        wanted: String,
        rejected: Vec<Rejected>,
        help: String,
    },

    #[error(
        "`{}` has no libc++ module sources (libc++.modules.json)\n\n\
         help: install libc++ for Clang {major} (on Debian/Ubuntu: libc++-{major}-dev \
         and libc++abi-{major}-dev)",
        cxx.display()
    )]
    NoStdModules { cxx: PathBuf, major: u32 },

    #[error("failed to read `{}`: {reason}", manifest.display())]
    StdManifest { manifest: PathBuf, reason: String },

    #[error(
        "`{}` lists the `std` module at `{}`, which doesn't exist\n\n\
         help: reinstall libc++ for this Clang",
        manifest.display(),
        source_path.display()
    )]
    StdSourceMissing {
        manifest: PathBuf,
        source_path: PathBuf,
    },
}

fn not_found(tool: &str, wanted: &str, rejected: &[Rejected], help: &str) -> String {
    let mut message = format!("could not find {tool} ({wanted})");
    if !rejected.is_empty() {
        message.push('\n');
        for candidate in rejected {
            let _ = write!(
                message,
                "\n  skipped `{}`: {}",
                candidate.path.display(),
                candidate.reason
            );
        }
    }
    message.push_str("\n\nhelp: ");
    message.push_str(help);
    message
}

impl Toolchain {
    /// Locate the toolchain for this process's environment.
    ///
    /// # Errors
    ///
    /// Fails if a tool is missing or too old, an override points at nothing usable,
    /// or libc++'s module sources can't be found.
    pub fn discover() -> Result<Self, ToolchainError> {
        let search = Search::from_env();
        let cxx = find_cxx(&search, &run_version)?;
        let scan_deps = find_scan_deps(&search, &cxx, &run_version)?;
        let std_modules = std_modules::locate(&cxx)?;
        Ok(Self {
            cxx,
            scan_deps,
            std_modules,
        })
    }
}

/// Runs a tool's `--version`. Tests substitute a fake that reads the file instead.
type Probe<'a> = &'a dyn Fn(&Path) -> Result<String, String>;

fn run_version(tool: &Path) -> Result<String, String> {
    let output = Command::new(tool)
        .arg("--version")
        .output()
        .map_err(|err| format!("failed to run it: {err}"))?;
    if !output.status.success() {
        return Err(format!("`--version` failed ({})", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What a candidate's version must satisfy.
struct Requirement<'a> {
    describe: String,
    accepts: &'a dyn Fn(Version) -> bool,
}

/// Probe `path`: the tool if it's acceptable, otherwise why not.
fn check(path: &Path, requirement: &Requirement, probe: Probe) -> Result<Tool, String> {
    let output = probe(path)?;
    let version =
        Version::parse(&output).ok_or_else(|| "unrecognized `--version` output".to_string())?;
    if (requirement.accepts)(version) {
        Ok(Tool {
            path: path.to_path_buf(),
            version,
        })
    } else {
        Err(format!("version {version}, need {}", requirement.describe))
    }
}

/// Try `candidates` in order, skipping missing files.
fn first_acceptable(
    candidates: impl IntoIterator<Item = PathBuf>,
    requirement: &Requirement,
    probe: Probe,
    rejected: &mut Vec<Rejected>,
) -> Option<Tool> {
    for path in candidates {
        if !path.is_file() || rejected.iter().any(|r| r.path == path) {
            continue;
        }
        match check(&path, requirement, probe) {
            Ok(tool) => return Some(tool),
            Err(reason) => rejected.push(Rejected { path, reason }),
        }
    }
    None
}

/// Use the override in `var`: a path, or a name looked up on `PATH`.
fn from_override(
    var: &'static str,
    value: &OsStr,
    search: &Search,
    requirement: &Requirement,
    probe: Probe,
) -> Result<Tool, ToolchainError> {
    let error = |reason: String| ToolchainError::Override {
        var,
        value: PathBuf::from(value),
        reason,
    };
    let given = Path::new(value);
    let path = if given.components().count() > 1 || given.is_absolute() {
        given
            .is_file()
            .then(|| given.to_path_buf())
            .ok_or_else(|| error("that file doesn't exist".into()))?
    } else {
        let name = exe(&value.to_string_lossy());
        search
            .path
            .iter()
            .map(|dir| dir.join(&name))
            .find(|path| path.is_file())
            .ok_or_else(|| error("it isn't on PATH".into()))?
    };
    check(&path, requirement, probe).map_err(|reason| error(format!("it was skipped: {reason}")))
}

/// `name` with the platform's executable suffix.
fn exe(name: &str) -> String {
    let suffix = env::consts::EXE_SUFFIX;
    if suffix.is_empty() || name.ends_with(suffix) {
        name.to_string()
    } else {
        format!("{name}{suffix}")
    }
}

/// Every `dir/name`, names outermost.
fn in_dirs<'a>(names: &'a [String], dirs: &'a [PathBuf]) -> impl Iterator<Item = PathBuf> + 'a {
    names
        .iter()
        .flat_map(move |name| dirs.iter().map(move |dir| dir.join(name)))
}

fn find_cxx(search: &Search, probe: Probe) -> Result<Tool, ToolchainError> {
    let requirement = Requirement {
        describe: format!("{MIN_CLANG_MAJOR} or newer"),
        accepts: &|version| version.major >= MIN_CLANG_MAJOR,
    };
    if let Some(value) = &search.cxx_override {
        return from_override(CXX_ENV, value, search, &requirement, probe);
    }

    let names: Vec<String> = std::iter::once(exe("clang++"))
        .chain(
            (MIN_CLANG_MAJOR..=MIN_CLANG_MAJOR + NEWER_MAJORS)
                .rev()
                .map(|major| exe(&format!("clang++-{major}"))),
        )
        .collect();
    let bundled: Vec<PathBuf> = search.bundled_dir.iter().cloned().collect();
    let candidates = in_dirs(&names[..1], &bundled).chain(in_dirs(&names, &search.path));

    let mut rejected = Vec::new();
    first_acceptable(candidates, &requirement, probe, &mut rejected).ok_or_else(|| {
        ToolchainError::NotFound {
            tool: "clang++",
            wanted: format!("Clang {MIN_CLANG_MAJOR} or newer"),
            rejected,
            help: format!(
                "install Clang {MIN_CLANG_MAJOR} or newer, or set {CXX_ENV} to its clang++"
            ),
        }
    })
}

fn find_scan_deps(search: &Search, cxx: &Tool, probe: Probe) -> Result<Tool, ToolchainError> {
    let major = cxx.version.major;
    let requirement = Requirement {
        describe: format!("{major}.x to match clang++"),
        accepts: &|version| version.major == major,
    };
    if let Some(value) = &search.scan_deps_override {
        return from_override(SCAN_DEPS_ENV, value, search, &requirement, probe);
    }

    // `clang++-22` pairs with `clang-scan-deps-22`. Look next to both the path as
    // found and its real location (e.g. `/usr/bin/clang++-22` is a symlink into
    // `/usr/lib/llvm-22/bin/`).
    let suffix = cxx
        .path
        .file_stem()
        .and_then(OsStr::to_str)
        .and_then(|stem| stem.strip_prefix("clang++"))
        .unwrap_or_default();
    let sibling_names: Vec<String> = [format!("clang-scan-deps{suffix}"), "clang-scan-deps".into()]
        .iter()
        .map(|name| exe(name))
        .collect();
    let mut sibling_dirs: Vec<PathBuf> = cxx
        .path
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    if let Some(real) = fs::canonicalize(&cxx.path)
        .ok()
        .and_then(|real| real.parent().map(Path::to_path_buf))
        && !sibling_dirs.contains(&real)
    {
        sibling_dirs.push(real);
    }

    let names = [
        exe(&format!("clang-scan-deps-{major}")),
        exe("clang-scan-deps"),
    ];
    let bundled: Vec<PathBuf> = search.bundled_dir.iter().cloned().collect();
    let candidates = in_dirs(&sibling_names, &sibling_dirs)
        .chain(in_dirs(&names[1..], &bundled))
        .chain(in_dirs(&names, &search.path));

    let mut rejected = Vec::new();
    first_acceptable(candidates, &requirement, probe, &mut rejected).ok_or_else(|| {
        ToolchainError::NotFound {
            tool: "clang-scan-deps",
            wanted: format!("Clang {major}, to match `{}`", cxx.path.display()),
            rejected,
            help: format!(
                "install clang-scan-deps for Clang {major} (on Debian/Ubuntu: \
                 clang-tools-{major}), or set {SCAN_DEPS_ENV} to it"
            ),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Reads a fake tool, whose content is its `--version` output.
    fn fake_probe(path: &Path) -> Result<String, String> {
        fs::read_to_string(path).map_err(|err| err.to_string())
    }

    /// Writes a fake tool `name` in `dir` reporting `version`.
    fn tool(dir: &Path, name: &str, version: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(exe(name));
        fs::write(&path, format!("Ubuntu clang version {version} (fake)\n")).unwrap();
        path
    }

    fn search(path: &[&Path]) -> Search {
        Search {
            path: path.iter().map(|dir| dir.to_path_buf()).collect(),
            ..Search::default()
        }
    }

    fn cxx(search: &Search) -> Result<Tool, ToolchainError> {
        find_cxx(search, &fake_probe)
    }

    #[test]
    fn parses_versions() {
        let v = |major, minor, patch| Version {
            major,
            minor,
            patch,
        };
        assert_eq!(
            Version::parse("Ubuntu clang version 22.1.8 (++2026)\nTarget: x"),
            Some(v(22, 1, 8))
        );
        assert_eq!(
            Version::parse("Ubuntu LLVM version 22.1.8\n  Optimized build."),
            Some(v(22, 1, 8))
        );
        assert_eq!(Version::parse("clang version 23.0.0git"), Some(v(23, 0, 0)));
        assert_eq!(
            Version::parse("Homebrew clang version 22"),
            Some(v(22, 0, 0))
        );
        assert_eq!(Version::parse("gcc (GCC) 14.2.0"), None);
        assert_eq!(v(22, 1, 8).to_string(), "22.1.8");
    }

    #[test]
    fn skips_an_old_clang_for_a_versioned_one() {
        let temp = tempdir().unwrap();
        let old = tool(&temp.path().join("bin"), "clang++", "18.1.3");
        let new = tool(&temp.path().join("bin"), "clang++-22", "22.1.8");
        let found = cxx(&search(&[&temp.path().join("bin")])).unwrap();
        assert_eq!(found.path, new);
        assert_ne!(found.path, old);
    }

    #[test]
    fn prefers_the_unversioned_name_and_earlier_path_entries() {
        let temp = tempdir().unwrap();
        let (a, b) = (temp.path().join("a"), temp.path().join("b"));
        tool(&a, "clang++-23", "23.0.0");
        let plain = tool(&b, "clang++", "22.1.8");
        assert_eq!(cxx(&search(&[&a, &b])).unwrap().path, plain);

        let first = tool(&a, "clang++", "22.1.0");
        assert_eq!(cxx(&search(&[&a, &b])).unwrap().path, first);
    }

    #[test]
    fn bundled_toolchain_comes_before_path() {
        let temp = tempdir().unwrap();
        let bundled = tool(&temp.path().join("libexec"), "clang++", "22.1.8");
        tool(&temp.path().join("bin"), "clang++", "22.1.8");
        let mut search = search(&[&temp.path().join("bin")]);
        search.bundled_dir = Some(temp.path().join("libexec"));
        assert_eq!(cxx(&search).unwrap().path, bundled);
    }

    #[test]
    fn bundled_dir_follows_the_release_layout() {
        let exe = Path::new("root").join("bin").join("yam");
        let expected = if cfg!(windows) {
            Path::new("root").join("bin")
        } else {
            Path::new("root").join("bin/../libexec/yam")
        };
        assert_eq!(bundled_dir(&exe), Some(expected));
    }

    #[test]
    fn override_wins_and_is_checked() {
        let temp = tempdir().unwrap();
        let bin = temp.path().join("bin");
        tool(&bin, "clang++", "22.1.8");
        let custom = tool(&temp.path().join("custom"), "my-clang", "23.1.0");

        let mut search = search(&[&bin]);
        search.cxx_override = Some(custom.clone().into());
        assert_eq!(cxx(&search).unwrap().path, custom);

        search.cxx_override = Some(exe("clang++").into());
        assert_eq!(cxx(&search).unwrap().path, bin.join(exe("clang++")));

        search.cxx_override = Some(temp.path().join("missing").into());
        let err = cxx(&search).unwrap_err().to_string();
        assert!(err.starts_with("YAM_CXX is set to"), "{err}");
        assert!(err.ends_with("that file doesn't exist"), "{err}");

        search.cxx_override = Some("no-such-clang".into());
        let err = cxx(&search).unwrap_err().to_string();
        assert!(err.ends_with("it isn't on PATH"), "{err}");

        let old = tool(&temp.path().join("old"), "clang++", "18.1.3");
        search.cxx_override = Some(old.into());
        let err = cxx(&search).unwrap_err().to_string();
        assert!(err.ends_with("version 18.1.3, need 22 or newer"), "{err}");
    }

    #[test]
    fn missing_clang_is_actionable() {
        let temp = tempdir().unwrap();
        let old = tool(temp.path(), "clang++", "18.1.3");
        let err = cxx(&search(&[temp.path()])).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "could not find clang++ (Clang 22 or newer)\n\n  skipped `{}`: version \
                 18.1.3, need 22 or newer\n\nhelp: install Clang 22 or newer, or set \
                 YAM_CXX to its clang++",
                old.display()
            )
        );
    }

    #[test]
    fn scan_deps_comes_from_the_same_install() {
        let temp = tempdir().unwrap();
        let llvm = temp.path().join("llvm-22").join("bin");
        let bin = temp.path().join("bin");
        let clang = tool(&llvm, "clang++", "22.1.8");
        let sibling = tool(&llvm, "clang-scan-deps", "22.1.8");
        tool(&bin, "clang-scan-deps", "22.1.8");
        let cxx = check(&clang, &any_version(), &fake_probe).unwrap();
        let found = find_scan_deps(&search(&[&bin]), &cxx, &fake_probe).unwrap();
        assert_eq!(found.path, sibling);
    }

    #[test]
    fn scan_deps_keeps_the_versioned_suffix_and_matches_the_major() {
        let temp = tempdir().unwrap();
        let bin = temp.path().join("bin");
        let clang = tool(&bin, "clang++-22", "22.1.8");
        tool(&bin, "clang-scan-deps", "18.1.3");
        let versioned = tool(&bin, "clang-scan-deps-22", "22.1.8");
        let cxx = check(&clang, &any_version(), &fake_probe).unwrap();
        assert_eq!(
            find_scan_deps(&search(&[&bin]), &cxx, &fake_probe)
                .unwrap()
                .path,
            versioned
        );

        fs::remove_file(&versioned).unwrap();
        let err = find_scan_deps(&search(&[&bin]), &cxx, &fake_probe)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("version 18.1.3, need 22.x to match clang++"),
            "{err}"
        );
        assert!(err.contains("clang-tools-22"), "{err}");
    }

    fn any_version() -> Requirement<'static> {
        Requirement {
            describe: String::new(),
            accepts: &|_| true,
        }
    }

    /// Runs discovery against the real environment. CI installs Clang 22 with libc++
    /// on Linux and macOS, so there it must succeed; elsewhere a missing toolchain
    /// only skips the test.
    #[test]
    fn discovers_the_installed_toolchain() {
        match Toolchain::discover() {
            Ok(toolchain) => {
                eprintln!("{toolchain:#?}");
                assert!(toolchain.cxx.version.major >= MIN_CLANG_MAJOR);
                assert_eq!(
                    toolchain.scan_deps.version.major,
                    toolchain.cxx.version.major
                );
                assert!(toolchain.std_modules.std.source.is_file());
            }
            Err(err) if env::var_os("CI").is_some() && !cfg!(windows) => {
                panic!("toolchain discovery failed in CI: {err}");
            }
            Err(err) => eprintln!("skipping: {err}"),
        }
    }
}
