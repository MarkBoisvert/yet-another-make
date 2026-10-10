//! `yam build` for single-file targets: one source file per lib or bin, built in
//! order with no scanning or staleness tracking beyond `std.pcm`. Multi-file targets
//! (#20), dependency scanning (#21) and the build graph (#24) replace most of this.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::cli::BuildArgs;
use crate::manifest::{
    self, CppStd, GlobList, Manifest, SourceTree, Target, TargetKind, has_errors, report,
    resolve_targets, validate,
};
use crate::style;
use crate::toolchain::Toolchain;

/// The build profile, each with its own `target/<Profile>/` directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}

impl Profile {
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Debug => "Debug",
            Self::Release => "Release",
        }
    }

    fn flags(self) -> &'static [&'static str] {
        match self {
            Self::Debug => &["-O0", "-g"],
            Self::Release => &["-O3", "-DNDEBUG"],
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Debug => "unoptimized + debuginfo",
            Self::Release => "optimized",
        }
    }
}

/// Build the project at `args.path`.
///
/// # Errors
///
/// Fails if the manifest is invalid, a selected target doesn't exist or has more
/// than one source file, the toolchain can't be found, or a compile, archive or link
/// step fails.
pub fn run(args: &BuildArgs) -> Result<()> {
    let start = Instant::now();
    let root = args
        .path
        .canonicalize()
        .with_context(|| format!("failed to resolve `{}`", args.path.display()))?;
    let manifest = Manifest::load(&root)?;
    let diagnostics = validate(&manifest, &root)?;
    report(&diagnostics);
    if has_errors(&diagnostics) {
        bail!(
            "could not build `{}` due to errors in `{}`",
            manifest.project.name,
            manifest::MANIFEST_FILE_NAME
        );
    }
    if !manifest.dependencies.is_empty() {
        style::warning(
            "[dependencies] are ignored for now; dependency support comes with packages",
        );
    }

    let targets = select(resolve_targets(&manifest, &root)?, args)?;
    let tree = SourceTree::scan(&root)?;
    let sources = targets
        .iter()
        .map(|target| single_source(target, &tree))
        .collect::<Result<Vec<_>>>()?;

    let profile = if args.release {
        Profile::Release
    } else {
        Profile::Debug
    };
    let toolchain = Toolchain::discover()?;

    style::status(
        "Building",
        format!(
            "{} v{} ({})",
            manifest.project.name,
            manifest.project.version,
            root.display()
        ),
    );
    let builder = Builder {
        root: &root,
        out: root.join("target").join(profile.dir_name()),
        project: &manifest.project.name,
        std: manifest.project.std,
        profile,
        toolchain: &toolchain,
        verbose: args.verbose,
    };
    let std_module = if builder.std.supports_import_std() {
        Some(builder.std_module()?)
    } else {
        None
    };

    let mut lib = None;
    for (target, source) in targets.iter().zip(&sources) {
        match target.kind {
            TargetKind::Lib => lib = Some(builder.lib(target, source, std_module.as_ref())?),
            TargetKind::Bin => {
                let lib = lib.as_ref().filter(|_| target.links_lib);
                builder.bin(target, source, std_module.as_ref(), lib)?;
            }
        }
    }

    style::status(
        "Finished",
        format!(
            "`{}` profile [{}] in {:.2}s",
            profile.dir_name(),
            profile.description(),
            start.elapsed().as_secs_f64()
        ),
    );
    Ok(())
}

/// The targets named by `--lib`/`--bin`, or all of them. A selected bin that links
/// the project library brings the library along. Keeps `targets`' order, which has
/// the library first.
fn select(targets: Vec<Target>, args: &BuildArgs) -> Result<Vec<Target>> {
    if !args.lib && args.bins.is_empty() {
        return Ok(targets);
    }
    let has_lib = targets.iter().any(|t| t.kind == TargetKind::Lib);
    if args.lib && !has_lib {
        bail!("no library target in this project");
    }
    for name in &args.bins {
        if !targets
            .iter()
            .any(|t| t.kind == TargetKind::Bin && &t.name == name)
        {
            let available: Vec<_> = targets
                .iter()
                .filter(|t| t.kind == TargetKind::Bin)
                .map(|t| format!("`{}`", t.name))
                .collect();
            let available = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            bail!("no bin target named `{name}`\n\nhelp: available bin targets: {available}");
        }
    }
    let bin_selected = |t: &Target| t.kind == TargetKind::Bin && args.bins.contains(&t.name);
    let needs_lib = args.lib || targets.iter().any(|t| bin_selected(t) && t.links_lib);
    Ok(targets
        .into_iter()
        .filter(|t| bin_selected(t) || (t.kind == TargetKind::Lib && needs_lib))
        .collect())
}

/// The target's one source file, relative to the root.
fn single_source(target: &Target, tree: &SourceTree) -> Result<PathBuf> {
    let files = tree.select(
        &GlobList::new(&target.sources)?,
        &GlobList::new(&target.exclude)?,
    );
    let kind = kind_name(target.kind);
    match files.as_slice() {
        [file] => Ok(PathBuf::from(file)),
        [] => bail!("{kind} target `{}` has no source files", target.name),
        files => bail!(
            "{kind} target `{}` has {} source files, but yam can only build \
             single-file targets so far\n\nhelp: multi-file targets are coming (#20)",
            target.name,
            files.len()
        ),
    }
}

fn kind_name(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Lib => "lib",
        TargetKind::Bin => "bin",
    }
}

/// The compiled `std` module, shared by every target at one standard.
struct StdModule {
    pcm: PathBuf,
    object: PathBuf,
}

/// A built library.
struct Lib {
    archive: PathBuf,
    /// For a module library, its name and compiled interface.
    module: Option<(String, PathBuf)>,
    include_dirs: Vec<PathBuf>,
}

struct Builder<'a> {
    root: &'a Path,
    out: PathBuf,
    project: &'a str,
    std: CppStd,
    profile: Profile,
    toolchain: &'a Toolchain,
    verbose: bool,
}

impl Builder<'_> {
    /// `clang++` with the flags every compile shares.
    fn compile_command(&self) -> Command {
        let mut command = Command::new(&self.toolchain.cxx.path);
        command
            .arg(format!("-std={}", self.std.as_str()))
            .arg("-stdlib=libc++")
            .args(self.profile.flags());
        command
    }

    /// Build `std.pcm` and `std.o` into `std/<std>/`, unless the last build left them
    /// there from the same compiler, commands and `std.cppm`.
    fn std_module(&self) -> Result<StdModule> {
        let dir = self.out.join("std").join(self.std.as_str());
        let module = StdModule {
            pcm: dir.join("std.pcm"),
            object: dir.join("std.o"),
        };
        let std = &self.toolchain.std_modules.std;

        let mut precompile = self.compile_command();
        precompile.arg("-Wno-reserved-module-identifier");
        for include in &std.system_include_dirs {
            precompile.arg("-isystem").arg(include);
        }
        precompile
            .arg("--precompile")
            .arg(&std.source)
            .arg("-o")
            .arg(&module.pcm);
        // Compiling a `.pcm` to an object reads no headers, so no `-stdlib`.
        let mut compile = Command::new(&self.toolchain.cxx.path);
        compile
            .arg(format!("-std={}", self.std.as_str()))
            .args(self.profile.flags())
            .arg("-c")
            .arg(&module.pcm)
            .arg("-o")
            .arg(&module.object);

        // Until the build state file (#24, #26), a stamp next to the outputs records
        // what they were built from.
        let stamp_path = dir.join("std.stamp");
        let stamp = [
            identity(&self.toolchain.cxx.path),
            self.toolchain.cxx.version.to_string(),
            identity(&std.source),
            display(&precompile),
            display(&compile),
        ]
        .join("\n");
        let fresh = module.pcm.is_file()
            && module.object.is_file()
            && fs::read_to_string(&stamp_path).is_ok_and(|old| old == stamp);
        if fresh {
            return Ok(module);
        }

        create_dir(&dir)?;
        // Remove the stamp first so an interrupted build can't look fresh.
        let _ = fs::remove_file(&stamp_path);
        self.run(&mut precompile, "failed to build the `std` module")?;
        self.run(&mut compile, "failed to build the `std` module")?;
        fs::write(&stamp_path, stamp)
            .with_context(|| format!("failed to write `{}`", stamp_path.display()))?;
        Ok(module)
    }

    fn lib(&self, target: &Target, source: &Path, std: Option<&StdModule>) -> Result<Lib> {
        let object = self.object_path(source)?;
        let include_dirs: Vec<PathBuf> = target
            .include_dirs
            .iter()
            .map(|dir| self.root.join(dir))
            .collect();
        let mut compile = self.compile_command();
        add_std(&mut compile, std);
        add_includes(&mut compile, &include_dirs);

        let is_module = source.extension().is_some_and(|ext| ext == "cppm");
        let module = if is_module {
            let text = fs::read_to_string(self.root.join(source))
                .with_context(|| format!("failed to read `{}`", source.display()))?;
            let name = module_name(&text).with_context(|| {
                format!(
                    "`{}` has no `export module <name>;` declaration",
                    source.display()
                )
            })?;
            let dir = self.out.join("modules").join(self.project);
            create_dir(&dir)?;
            let pcm = dir.join(format!("{name}.pcm"));
            compile.arg(format!("-fmodule-output={}", pcm.display()));
            Some((name, pcm))
        } else {
            None
        };
        compile
            .arg("-c")
            .arg(self.root.join(source))
            .arg("-o")
            .arg(&object);
        self.run(&mut compile, &could_not("compile", target))?;

        let archive = self.out.join(format!("lib{}.a", target.name));
        let _ = fs::remove_file(&archive);
        let mut ar = Command::new(&self.toolchain.ar);
        ar.arg("rcs").arg(&archive).arg(&object);
        self.run(&mut ar, &could_not("archive", target))?;

        Ok(Lib {
            archive,
            module,
            include_dirs,
        })
    }

    fn bin(
        &self,
        target: &Target,
        source: &Path,
        std: Option<&StdModule>,
        lib: Option<&Lib>,
    ) -> Result<()> {
        let object = self.object_path(source)?;
        let mut include_dirs: Vec<PathBuf> = target
            .include_dirs
            .iter()
            .map(|dir| self.root.join(dir))
            .collect();
        let mut compile = self.compile_command();
        add_std(&mut compile, std);
        if let Some(lib) = lib {
            include_dirs.extend(lib.include_dirs.iter().cloned());
            if let Some((name, pcm)) = &lib.module {
                compile.arg(format!("-fmodule-file={name}={}", pcm.display()));
            }
        }
        add_includes(&mut compile, &include_dirs);
        compile
            .arg("-c")
            .arg(self.root.join(source))
            .arg("-o")
            .arg(&object);
        self.run(&mut compile, &could_not("compile", target))?;

        let executable = self
            .out
            .join(format!("{}{}", target.name, std::env::consts::EXE_SUFFIX));
        let mut link = Command::new(&self.toolchain.cxx.path);
        link.arg("-stdlib=libc++").arg(&object);
        if let Some(lib) = lib {
            link.arg(&lib.archive);
        }
        if let Some(std) = std {
            link.arg(&std.object);
        }
        // Link against the libc++ that matches the headers, not whichever one the
        // linker finds first (Homebrew's lives outside the default search path).
        if let Some(dir) = libcxx_dir(self.toolchain) {
            link.arg(format!("-L{}", dir.display()));
            if cfg!(target_os = "macos") {
                link.arg(format!("-Wl,-rpath,{}", dir.display()));
            }
        }
        link.arg("-o").arg(&executable);
        self.run(&mut link, &could_not("link", target))
    }

    /// `obj/<project>/<source>.o`, creating its directory.
    fn object_path(&self, source: &Path) -> Result<PathBuf> {
        let mut path = self.out.join("obj").join(self.project).join(source);
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".o");
        path.set_file_name(name);
        if let Some(dir) = path.parent() {
            create_dir(dir)?;
        }
        Ok(path)
    }

    /// Run `command`, echoing it with `-v`. Its output goes straight to the terminal.
    fn run(&self, command: &mut Command, failure: &str) -> Result<()> {
        if self.verbose {
            style::status("Running", format!("`{}`", display(command)));
        }
        let status = command
            .status()
            .with_context(|| format!("{failure}: couldn't run `{}`", display(command)))?;
        if !status.success() {
            bail!("{failure}");
        }
        Ok(())
    }
}

fn could_not(verb: &str, target: &Target) -> String {
    format!(
        "could not {verb} `{}` ({})",
        target.name,
        kind_name(target.kind)
    )
}

fn add_std(command: &mut Command, std: Option<&StdModule>) {
    if let Some(std) = std {
        command.arg(format!("-fmodule-file=std={}", std.pcm.display()));
    }
}

fn add_includes(command: &mut Command, dirs: &[PathBuf]) {
    for dir in dirs {
        command.arg(format!("-I{}", dir.display()));
    }
}

/// The directory holding libc++ itself: the one with `libc++.modules.json` in it.
fn libcxx_dir(toolchain: &Toolchain) -> Option<PathBuf> {
    let manifest = &toolchain.std_modules.manifest;
    fs::canonicalize(manifest)
        .unwrap_or_else(|_| manifest.clone())
        .parent()
        .map(Path::to_path_buf)
}

/// The name in a module interface's `export module <name>;`. A stopgap until
/// `clang-scan-deps` reports it (#21).
fn module_name(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix("export")?;
        let rest = rest.trim_start().strip_prefix("module")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let name = rest.trim_start().split(';').next()?.trim();
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':'));
        valid.then(|| name.to_string())
    })
}

/// A file's path, size and modification time, so a replaced file looks different.
fn identity(path: &Path) -> String {
    let meta = fs::metadata(path).ok();
    let size = meta.as_ref().map_or(0, fs::Metadata::len);
    let modified = meta
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    format!("{} {size} {modified}", path.display())
}

/// A command as a shell-like line, quoting arguments with spaces.
fn display(command: &Command) -> String {
    let mut out = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        let arg = arg.to_string_lossy();
        if arg.contains(char::is_whitespace) || arg.is_empty() {
            let _ = write!(out, " \"{arg}\"");
        } else {
            let _ = write!(out, " {arg}");
        }
    }
    out
}

fn create_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create `{}`", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{InitArgs, Vcs};
    use crate::commands::init;
    use tempfile::tempdir;

    fn build_args(path: &Path) -> BuildArgs {
        BuildArgs {
            path: path.to_path_buf(),
            release: false,
            verbose: true,
            lib: false,
            bins: Vec::new(),
        }
    }

    fn new_project(dir: &Path, lib: bool, legacy: bool) {
        init::run(&InitArgs {
            path: dir.to_path_buf(),
            bin: false,
            lib,
            name: None,
            legacy,
            vcs: Some(Vcs::None),
        })
        .unwrap();
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

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

    fn run_binary(path: &Path) -> String {
        let output = Command::new(path).output().unwrap();
        assert!(output.status.success(), "{} failed", path.display());
        String::from_utf8(output.stdout).unwrap()
    }

    fn exe(dir: &Path, profile: &str, name: &str) -> PathBuf {
        dir.join("target")
            .join(profile)
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
    }

    #[test]
    fn builds_and_runs_the_import_std_template() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("app");
        new_project(&dir, false, false);
        run(&build_args(&dir)).unwrap();
        assert_eq!(run_binary(&exe(&dir, "Debug", "app")), "Hello, world!\n");

        // The second build reuses `std.pcm`.
        let pcm = dir.join("target/Debug/std/c++26/std.pcm");
        let built = fs::metadata(&pcm).unwrap().modified().unwrap();
        run(&build_args(&dir)).unwrap();
        assert_eq!(fs::metadata(&pcm).unwrap().modified().unwrap(), built);
    }

    #[test]
    fn release_builds_the_legacy_template() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("old-app");
        new_project(&dir, false, true);
        let mut args = build_args(&dir);
        args.release = true;
        run(&args).unwrap();
        assert_eq!(
            run_binary(&exe(&dir, "Release", "old-app")),
            "Hello, world!\n"
        );
        assert!(!dir.join("target/Debug").exists());
    }

    #[test]
    fn bin_imports_the_project_module_library() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("calc");
        new_project(&dir, true, false);
        write(
            &dir.join("src/main.cpp"),
            "import std;\nimport calc;\n\nint main() {\n    std::println(\"{}\", calc::add(40, 2));\n}\n",
        );
        run(&build_args(&dir)).unwrap();
        assert!(dir.join("target/Debug/libcalc.a").is_file());
        assert!(dir.join("target/Debug/modules/calc/calc.pcm").is_file());
        assert_eq!(run_binary(&exe(&dir, "Debug", "calc")), "42\n");
    }

    #[test]
    fn bin_uses_the_legacy_library_header() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("old-calc");
        new_project(&dir, true, true);
        write(
            &dir.join("src/main.cpp"),
            "#include <cstdio>\n#include \"old-calc.hpp\"\n\nint main() {\n    std::printf(\"%d\\n\", old_calc::add(40, 2));\n}\n",
        );
        let mut args = build_args(&dir);
        args.bins = vec!["old-calc".into()];
        run(&args).unwrap();
        assert!(dir.join("target/Debug/libold-calc.a").is_file());
        assert_eq!(run_binary(&exe(&dir, "Debug", "old-calc")), "42\n");
    }

    #[test]
    fn pre_cxx23_projects_skip_the_std_module() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("c17");
        write(
            &dir.join("Yam.toml"),
            "[project]\nname = \"c17\"\nstd = \"c++17\"\n",
        );
        write(
            &dir.join("src/main.cpp"),
            "#include <cstdio>\n#include <optional>\n\nint main() {\n    std::printf(\"%d\\n\", std::optional<int>(17).value());\n}\n",
        );
        run(&build_args(&dir)).unwrap();
        assert_eq!(run_binary(&exe(&dir, "Debug", "c17")), "17\n");
        assert!(!dir.join("target/Debug/std").exists());
    }

    #[test]
    fn compile_errors_name_the_target() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("broken");
        new_project(&dir, false, false);
        write(&dir.join("src/main.cpp"), "int main() { return nope; }\n");
        let err = run(&build_args(&dir)).unwrap_err().to_string();
        assert_eq!(err, "could not compile `broken` (bin)");
    }

    fn target(name: &str, kind: TargetKind, links_lib: bool) -> Target {
        Target {
            name: name.into(),
            kind,
            entry: PathBuf::new(),
            sources: Vec::new(),
            exclude: Vec::new(),
            include_dirs: Vec::new(),
            links_lib,
        }
    }

    fn names(targets: &[Target]) -> Vec<&str> {
        targets.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn selects_targets_by_flag() {
        let all = vec![
            target("core", TargetKind::Lib, false),
            target("app", TargetKind::Bin, true),
            target("tool", TargetKind::Bin, true),
        ];
        let mut args = build_args(Path::new("."));
        assert_eq!(
            names(&select(all.clone(), &args).unwrap()),
            ["core", "app", "tool"]
        );

        args.bins = vec!["tool".into()];
        assert_eq!(
            names(&select(all.clone(), &args).unwrap()),
            ["core", "tool"]
        );

        args.bins.clear();
        args.lib = true;
        assert_eq!(names(&select(all.clone(), &args).unwrap()), ["core"]);

        args.lib = false;
        args.bins = vec!["nope".into()];
        let err = select(all, &args).unwrap_err().to_string();
        assert_eq!(
            err,
            "no bin target named `nope`\n\nhelp: available bin targets: `app`, `tool`"
        );

        args.bins.clear();
        args.lib = true;
        let err = select(vec![target("app", TargetKind::Bin, false)], &args).unwrap_err();
        assert_eq!(err.to_string(), "no library target in this project");
    }

    #[test]
    fn multi_file_targets_are_rejected() {
        let temp = tempdir().unwrap();
        write(&temp.path().join("src/lib.cppm"), "");
        write(&temp.path().join("src/detail.cpp"), "");
        let tree = SourceTree::scan(temp.path()).unwrap();
        let mut lib = target("core", TargetKind::Lib, false);
        lib.sources = vec!["src/**/*.cppm".into(), "src/**/*.cpp".into()];
        let err = single_source(&lib, &tree).unwrap_err().to_string();
        assert!(
            err.starts_with("lib target `core` has 2 source files"),
            "{err}"
        );

        lib.sources = vec!["src/lib.cppm".into()];
        assert_eq!(
            single_source(&lib, &tree).unwrap(),
            PathBuf::from("src/lib.cppm")
        );
    }

    #[test]
    fn reads_the_module_name() {
        assert_eq!(module_name("export module calc;\n"), Some("calc".into()));
        assert_eq!(
            module_name("module;\n#include <cstdio>\n  export   module a.b:part ;\n"),
            Some("a.b:part".into())
        );
        assert_eq!(module_name("// export module nope;\nmodule impl;\n"), None);
        assert_eq!(module_name("export modules x;\n"), None);
    }

    #[test]
    fn displays_commands() {
        let mut command = Command::new("clang++");
        command.args(["-c", "a b.cpp", ""]);
        assert_eq!(display(&command), "clang++ -c \"a b.cpp\" \"\"");
    }
}
