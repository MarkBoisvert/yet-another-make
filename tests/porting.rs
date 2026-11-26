//! Porting acceptance test (#20): an existing open-source, header-based C++ library
//! and its CLI build from a `Yam.toml` that only selects files (`sources`,
//! `exclude`, `include-dirs`), with no changes to the library's own sources.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use yet_another_make::cli::BuildArgs;
use yet_another_make::commands::build;
use yet_another_make::toolchain::Toolchain;

/// Whether a real toolchain is available. CI installs one on Linux and macOS, so
/// there a missing toolchain fails the test; elsewhere it skips it.
fn have_toolchain() -> bool {
    match Toolchain::discover() {
        Ok(_) => true,
        Err(err) if std::env::var_os("CI").is_some() && !cfg!(windows) => {
            panic!("toolchain discovery failed in CI: {err}")
        }
        Err(err) => {
            eprintln!("skipping: {err}");
            false
        }
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            copy_dir(&path, &to.join(entry.file_name()));
        } else {
            fs::copy(&path, to.join(entry.file_name())).unwrap();
        }
    }
}

#[test]
fn tinyxml2_and_its_cli_build_unmodified() {
    if !have_toolchain() {
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tinyxml2");
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("tinyxml2");
    copy_dir(&fixture, &dir);

    build::run(&BuildArgs {
        path: dir.clone(),
        release: false,
        verbose: false,
        lib: false,
        bins: Vec::new(),
    })
    .unwrap();

    let debug = dir.join("target/Debug");
    assert!(debug.join("libtinyxml2.a").is_file());
    let xmltest: PathBuf = debug.join(format!("xmltest{}", std::env::consts::EXE_SUFFIX));

    fs::write(
        dir.join("sample.xml"),
        "<?xml version=\"1.0\"?>\n<root><item id=\"1\">yam</item></root>\n",
    )
    .unwrap();
    let output = Command::new(&xmltest)
        .arg("sample.xml")
        .current_dir(&dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("Test file 'sample.xml' loaded. ErrorID=0\n"),
        "{stdout}"
    );

    fs::write(dir.join("broken.xml"), "<root><unclosed></root>\n").unwrap();
    let output = Command::new(&xmltest)
        .arg("broken.xml")
        .current_dir(&dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("Test file 'broken.xml' loaded. ErrorID=")
            && !stdout.starts_with("Test file 'broken.xml' loaded. ErrorID=0\n"),
        "{stdout}"
    );
}
