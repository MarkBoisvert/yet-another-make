//! Module dependencies from one batched `clang-scan-deps -format=p1689` call.
//!
//! Every file to scan becomes one entry in a generated compilation database, with the
//! same flags its compile uses and its object file as the entry's `output`. One
//! `clang-scan-deps` run scans them all, in parallel, and reports each file's
//! P1689 rule: the module it provides, if any, and the modules it requires. Rules
//! are matched back to files by their `primary-output`, which is the entry's
//! `output`; without that field `clang-scan-deps` leaves it empty.
//!
//! Each file is scanned on every build until the persistent scan cache (#22).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::{Deserialize, Serialize};

/// A file to scan, with the compile command it would be built with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanJob {
    /// The source file, absolute.
    pub source: PathBuf,
    /// Its object file, absolute. Unique per job, since it identifies the result.
    pub output: PathBuf,
    /// The full compile command, compiler first.
    pub arguments: Vec<OsString>,
}

/// What a file provides and requires.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deps {
    /// The module or partition the file provides (`m`, `m:part`), if any.
    pub provides: Option<Provided>,
    /// The modules and partitions it imports, partitions qualified as `m:part`. An
    /// implementation unit (`module m;`) requires `m`.
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provided {
    pub name: String,
    /// `false` for an internal partition (`module m:part;`).
    pub is_interface: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("failed to write `{}`", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("couldn't run `{}`", program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not scan the project's module dependencies ({status})")]
    Failed { status: ExitStatus },

    #[error("couldn't read `clang-scan-deps` output: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("`clang-scan-deps` reported nothing for `{}`", file.display())]
    Missing { file: PathBuf },
}

/// Scan `jobs` with the `clang-scan-deps` at `program`, writing the compilation
/// database to `database`. Returns each job's dependencies, in job order. The
/// scanner's diagnostics go straight to the terminal.
///
/// # Errors
///
/// Fails if the database can't be written, `clang-scan-deps` can't be run or fails
/// (for example on a syntax error in a module declaration, or a header-unit
/// import), or its output doesn't cover every job.
pub fn scan(
    program: &Path,
    database: &Path,
    jobs: &[ScanJob],
    verbose: bool,
) -> Result<Vec<Deps>, ScanError> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let write_error = |source| ScanError::Write {
        path: database.to_path_buf(),
        source,
    };
    if let Some(dir) = database.parent() {
        fs::create_dir_all(dir).map_err(write_error)?;
    }
    fs::write(database, compilation_database(jobs)).map_err(write_error)?;

    let mut command = Command::new(program);
    command
        .arg("-format=p1689")
        .arg(format!("-compilation-database={}", database.display()))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    if verbose {
        crate::style::status(
            "Running",
            format!(
                "`{} -format=p1689 -compilation-database={}`",
                program.display(),
                database.display()
            ),
        );
    }
    let output = command.output().map_err(|source| ScanError::Spawn {
        program: program.to_path_buf(),
        source,
    })?;
    if !output.status.success() {
        return Err(ScanError::Failed {
            status: output.status,
        });
    }
    match_rules(jobs, parse(&output.stdout)?)
}

/// The compilation database for `jobs`, as JSON.
#[must_use]
pub fn compilation_database(jobs: &[ScanJob]) -> String {
    #[derive(Serialize)]
    struct Entry {
        directory: String,
        file: String,
        output: String,
        arguments: Vec<String>,
    }
    let entries: Vec<Entry> = jobs
        .iter()
        .map(|job| Entry {
            directory: job
                .source
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .into_owned(),
            file: job.source.to_string_lossy().into_owned(),
            output: job.output.to_string_lossy().into_owned(),
            arguments: job
                .arguments
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        })
        .collect();
    serde_json::to_string_pretty(&entries).unwrap_or_default()
}

/// P1689 rules keyed by their `primary-output`.
fn parse(json: &[u8]) -> Result<HashMap<String, Deps>, serde_json::Error> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        rules: Vec<Rule>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "kebab-case")]
    struct Rule {
        #[serde(default)]
        primary_output: String,
        #[serde(default)]
        provides: Vec<Provide>,
        #[serde(default)]
        requires: Vec<Require>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "kebab-case")]
    struct Provide {
        logical_name: String,
        #[serde(default = "interface_by_default")]
        is_interface: bool,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "kebab-case")]
    struct Require {
        logical_name: String,
    }
    fn interface_by_default() -> bool {
        true
    }

    let file: File = serde_json::from_slice(json)?;
    Ok(file
        .rules
        .into_iter()
        .map(|rule| {
            let deps = Deps {
                provides: rule.provides.into_iter().next().map(|p| Provided {
                    name: p.logical_name,
                    is_interface: p.is_interface,
                }),
                requires: rule.requires.into_iter().map(|r| r.logical_name).collect(),
            };
            (normalize(&rule.primary_output), deps)
        })
        .collect())
}

/// Each job's rule, in job order.
fn match_rules(jobs: &[ScanJob], mut rules: HashMap<String, Deps>) -> Result<Vec<Deps>, ScanError> {
    jobs.iter()
        .map(|job| {
            rules
                .remove(&normalize(&job.output.to_string_lossy()))
                .ok_or_else(|| ScanError::Missing {
                    file: job.source.clone(),
                })
        })
        .collect()
}

/// A path as a lookup key, so `\` and `/` separators compare equal.
fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(source: &str, output: &str) -> ScanJob {
        ScanJob {
            source: PathBuf::from(source),
            output: PathBuf::from(output),
            arguments: vec!["clang++".into(), "-std=c++26".into(), "-c".into()],
        }
    }

    #[test]
    fn writes_one_database_entry_per_job() {
        let db = compilation_database(&[job("/p/src/a.cppm", "/p/out/a.cppm.o")]);
        let entries: serde_json::Value = serde_json::from_str(&db).unwrap();
        assert_eq!(
            entries,
            serde_json::json!([{
                "directory": "/p/src",
                "file": "/p/src/a.cppm",
                "output": "/p/out/a.cppm.o",
                "arguments": ["clang++", "-std=c++26", "-c"],
            }])
        );
    }

    /// Output in the shape `clang-scan-deps` 23 prints, rules in completion order.
    const OUTPUT: &str = r#"{
      "revision": 0,
      "rules": [
        { "primary-output": "/p/out/main.cpp.o",
          "requires": [ { "logical-name": "std" }, { "logical-name": "b", "source-path": "/p/b.cpp" } ] },
        { "primary-output": "/p/out/detail.cpp.o",
          "provides": [ { "is-interface": false, "logical-name": "a:detail", "source-path": "/p/detail.cpp" } ] },
        { "primary-output": "/p/out/a.cppm.o",
          "provides": [ { "is-interface": true, "logical-name": "a", "source-path": "/p/a.cppm" } ],
          "requires": [ { "logical-name": "a:part", "source-path": "/p/a-part.cppm" } ] },
        { "primary-output": "/p/out/plain.cpp.o" }
      ],
      "version": 1
    }"#;

    #[test]
    fn matches_rules_to_jobs_by_their_output() {
        let jobs = [
            job("/p/a.cppm", "/p/out/a.cppm.o"),
            job("/p/detail.cpp", "/p/out/detail.cpp.o"),
            job("/p/main.cpp", "/p/out/main.cpp.o"),
            job("/p/plain.cpp", "/p/out/plain.cpp.o"),
        ];
        let deps = match_rules(&jobs, parse(OUTPUT.as_bytes()).unwrap()).unwrap();
        assert_eq!(
            deps,
            [
                Deps {
                    provides: Some(Provided {
                        name: "a".into(),
                        is_interface: true
                    }),
                    requires: vec!["a:part".into()],
                },
                Deps {
                    provides: Some(Provided {
                        name: "a:detail".into(),
                        is_interface: false
                    }),
                    requires: vec![],
                },
                Deps {
                    provides: None,
                    requires: vec!["std".into(), "b".into()],
                },
                Deps::default(),
            ]
        );
    }

    #[test]
    fn a_job_without_a_rule_is_an_error() {
        let jobs = [job("/p/gone.cpp", "/p/out/gone.cpp.o")];
        let err = match_rules(&jobs, parse(OUTPUT.as_bytes()).unwrap()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "`clang-scan-deps` reported nothing for `/p/gone.cpp`"
        );
    }

    #[test]
    fn windows_separators_match() {
        let json = r#"{ "rules": [ { "primary-output": "C:\\p\\out\\a.cpp.o" } ] }"#;
        let jobs = [job("C:/p/a.cpp", "C:/p/out/a.cpp.o")];
        assert_eq!(
            match_rules(&jobs, parse(json.as_bytes()).unwrap()).unwrap(),
            [Deps::default()]
        );
    }
}
