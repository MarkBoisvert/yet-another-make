//! `yam build`: compile every file of the project's lib and bins, archive the lib and
//! link the bins.
//!
//! Every file is compiled on every build, one at a time, in the order
//! [`engine::plan`] gives. Only `std.pcm` is reused between builds so far. Batched
//! scanning (#21), staleness (#24) and the parallel scheduler (#25) replace most of
//! this.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::cli::BuildArgs;
use crate::engine::{self, Import, Plan, STD_MODULE, Scan, SourceFile};
use crate::manifest::{
    self, CppStd, GlobList, Manifest, SourceTree, Target, TargetKind, has_errors, is_link_path,
    report, resolve_targets, validate_tree,
};
use crate::style;
use crate::toolchain::Toolchain;

/// Extensions yam compiles.
const SOURCE_EXTENSIONS: [&str; 6] = ["cpp", "cc", "cxx", "c++", "cppm", "ixx"];
/// Extensions a source glob may pick up without being compiled.
const HEADER_EXTENSIONS: [&str; 9] = ["h", "hh", "hpp", "hxx", "h++", "inl", "ipp", "tpp", "inc"];
/// Above this many bytes of arguments, object lists go through a response file.
/// Windows limits a command line to 32 KiB.
const RESPONSE_FILE_THRESHOLD: usize = 8 * 1024;

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
/// Fails if the manifest is invalid, a selected target doesn't exist or has no source
/// files, its modules can't be ordered, the toolchain can't be found, or a compile,
/// archive or link step fails.
pub fn run(args: &BuildArgs) -> Result<()> {
    let start = Instant::now();
    let root = args
        .path
        .canonicalize()
        .with_context(|| format!("failed to resolve `{}`", args.path.display()))?;
    let manifest = Manifest::load(&root)?;
    let tree = SourceTree::scan(&root)?;
    let diagnostics = validate_tree(&manifest, &root, &tree)?;
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
    let std = manifest.project.std;
    let prepared = prepare(&root, &targets, &tree, std)?;
    let imports_std = prepared.iter().any(|p| p.plan.imports_std);

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
        std,
        profile,
        toolchain: &toolchain,
        verbose: args.verbose,
    };
    let std_module = if imports_std {
        Some(builder.std_module()?)
    } else {
        None
    };

    let mut lib = None;
    for target in &prepared {
        match target.target.kind {
            TargetKind::Lib => lib = Some(builder.lib(target, std_module.as_ref())?),
            TargetKind::Bin => {
                let lib = lib.as_ref().filter(|_| target.target.links_lib);
                builder.bin(target, std_module.as_ref(), lib)?;
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

/// A target with its files read and put in compile order.
struct Prepared<'t> {
    target: &'t Target,
    files: Vec<SourceFile>,
    plan: Plan,
}

impl Prepared<'_> {
    /// The modules and partitions the target provides.
    fn modules(&self) -> HashSet<String> {
        self.plan
            .steps
            .iter()
            .filter_map(|step| step.provides.clone())
            .collect()
    }
}

/// Read and order every target's files. Needs no toolchain, so a project's mistakes
/// are reported before any compiler runs.
fn prepare<'t>(
    root: &Path,
    targets: &'t [Target],
    tree: &SourceTree,
    std: CppStd,
) -> Result<Vec<Prepared<'t>>> {
    let mut prepared: Vec<Prepared<'t>> = Vec::with_capacity(targets.len());
    let mut lib_modules = HashSet::new();
    let none = HashSet::new();
    for target in targets {
        let files = source_files(root, target, tree)?;
        let available = if target.kind == TargetKind::Bin && target.links_lib {
            &lib_modules
        } else {
            &none
        };
        let plan = engine::plan(
            &describe(target),
            &files,
            target.kind == TargetKind::Bin,
            available,
        )?;
        if plan.imports_std && !std.supports_import_std() {
            let file = files
                .iter()
                .find(|f| f.scan.imports.contains(&Import::Module(STD_MODULE.into())))
                .map_or("", |f| f.path.as_str());
            bail!(
                "`{file}` imports `std`, which needs c++23 or later, but the project's \
                 std is {}\n\nhelp: set `std = \"c++23\"` or later in [project]",
                std.as_str()
            );
        }
        let target = Prepared {
            target,
            files,
            plan,
        };
        if target.target.kind == TargetKind::Lib {
            lib_modules = target.modules();
        }
        prepared.push(target);
    }
    Ok(prepared)
}

/// The target's files to compile, read and scanned, sorted by path.
fn source_files(root: &Path, target: &Target, tree: &SourceTree) -> Result<Vec<SourceFile>> {
    let selected = tree.select(
        &GlobList::new(&target.sources)?,
        &GlobList::new(&target.exclude)?,
    );
    let mut files = Vec::with_capacity(selected.len());
    for path in selected {
        let extension = Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default();
        if !SOURCE_EXTENSIONS.contains(&extension) {
            if !HEADER_EXTENSIONS.contains(&extension) {
                style::warning(format!(
                    "skipping `{path}` in {}: not a C++ source file",
                    describe(target)
                ));
            }
            continue;
        }
        let text = fs::read_to_string(root.join(path))
            .with_context(|| format!("failed to read `{path}`"))?;
        files.push(SourceFile {
            path: path.to_string(),
            scan: Scan::of(&text),
        });
    }
    if files.is_empty() {
        bail!("{} has no source files", describe(target));
    }
    Ok(files)
}

fn kind_name(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Lib => "lib",
        TargetKind::Bin => "bin",
    }
}

/// ``lib target `core` `` or ``bin target `app` ``.
fn describe(target: &Target) -> String {
    format!("{} target `{}`", kind_name(target.kind), target.name)
}

/// The compiled `std` module, shared by every target at one standard.
struct StdModule {
    pcm: PathBuf,
    object: PathBuf,
}

/// A built library, with what a bin needs to use and link it.
struct Lib {
    archive: PathBuf,
    /// Where its compiled module interfaces are, if it has any.
    module_dir: Option<PathBuf>,
    include_dirs: Vec<PathBuf>,
    links: Vec<String>,
    link_dirs: Vec<PathBuf>,
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

    /// Where a target's objects and compiled interfaces go: `obj/<project>/lib/` and
    /// `modules/<project>/` for the library, `…/bin/<name>/` for a bin.
    fn target_dirs(&self, target: &Target) -> (PathBuf, PathBuf) {
        let obj = self.out.join("obj").join(self.project);
        let modules = self.out.join("modules").join(self.project);
        match target.kind {
            TargetKind::Lib => (obj.join("lib"), modules),
            TargetKind::Bin => {
                let sub = Path::new("bin").join(&target.name);
                (obj.join(&sub), modules.join(sub))
            }
        }
    }

    /// Compile every file of `prepared` in plan order, returning the objects.
    fn compile(
        &self,
        prepared: &Prepared<'_>,
        std: Option<&StdModule>,
        lib: Option<&Lib>,
    ) -> Result<(Vec<PathBuf>, Option<PathBuf>)> {
        let target = prepared.target;
        let (obj_dir, module_dir) = self.target_dirs(target);
        let has_modules = prepared.plan.steps.iter().any(|s| s.provides.is_some());
        if has_modules {
            create_dir(&module_dir)?;
        }

        let mut include_dirs: Vec<PathBuf> = target
            .include_dirs
            .iter()
            .map(|dir| self.root.join(dir))
            .collect();
        let mut module_dirs = Vec::new();
        if has_modules {
            module_dirs.push(module_dir.clone());
        }
        if let Some(lib) = lib {
            include_dirs.extend(lib.include_dirs.iter().cloned());
            module_dirs.extend(lib.module_dir.iter().cloned());
        }

        let failure = could_not("compile", target);
        let mut objects = Vec::with_capacity(prepared.plan.steps.len());
        for step in &prepared.plan.steps {
            let source = &prepared.files[step.file].path;
            let object = object_path(&obj_dir, source)?;
            let mut compile = self.compile_command();
            if let Some(std) = std {
                compile.arg(format!("-fmodule-file=std={}", std.pcm.display()));
            }
            for dir in &module_dirs {
                compile.arg(format!("-fprebuilt-module-path={}", dir.display()));
            }
            for dir in &include_dirs {
                compile.arg(format!("-I{}", dir.display()));
            }
            if let Some(module) = &step.provides {
                let pcm = module_dir.join(pcm_name(module));
                compile.arg(format!("-fmodule-output={}", pcm.display()));
                // `.cppm` is a module interface by its extension; anything else that
                // declares a module needs saying so.
                if !engine::has_extension(source, "cppm") {
                    compile.args(["-x", "c++-module"]);
                }
            }
            compile
                .arg("-c")
                .arg(self.root.join(source))
                .arg("-o")
                .arg(&object);
            self.run(&mut compile, &failure)?;
            objects.push(object);
        }
        Ok((objects, has_modules.then_some(module_dir)))
    }

    fn lib(&self, prepared: &Prepared<'_>, std: Option<&StdModule>) -> Result<Lib> {
        let target = prepared.target;
        let (objects, module_dir) = self.compile(prepared, std, None)?;

        let archive = self.out.join(format!("lib{}.a", target.name));
        let _ = fs::remove_file(&archive);
        let mut ar = Command::new(&self.toolchain.ar);
        ar.arg("rcs").arg(&archive);
        self.add_files(&mut ar, &objects, &format!("lib{}.a", target.name))?;
        self.run(&mut ar, &could_not("archive", target))?;

        Ok(Lib {
            archive,
            module_dir,
            include_dirs: target
                .include_dirs
                .iter()
                .map(|dir| self.root.join(dir))
                .collect(),
            links: target.links.clone(),
            link_dirs: target.link_dirs.clone(),
        })
    }

    fn bin(
        &self,
        prepared: &Prepared<'_>,
        std: Option<&StdModule>,
        lib: Option<&Lib>,
    ) -> Result<()> {
        let target = prepared.target;
        let (mut objects, _) = self.compile(prepared, std, lib)?;
        if let Some(std) = std {
            objects.push(std.object.clone());
        }

        let executable = self
            .out
            .join(format!("{}{}", target.name, std::env::consts::EXE_SUFFIX));
        let mut link = Command::new(&self.toolchain.cxx.path);
        link.arg("-stdlib=libc++");
        self.add_files(&mut link, &objects, &target.name)?;
        if let Some(lib) = lib {
            link.arg(&lib.archive);
        }

        // The bin's own libraries, then the project library's, which its archive
        // needs but can't carry.
        let lib_links = lib.map(|lib| (&lib.links, &lib.link_dirs));
        for (links, link_dirs) in
            std::iter::once((&target.links, &target.link_dirs)).chain(lib_links)
        {
            for dir in link_dirs {
                link.arg(format!("-L{}", self.root.join(dir).display()));
            }
            for name in links {
                if is_link_path(name) {
                    link.arg(self.root.join(name));
                } else {
                    link.arg(format!("-l{name}"));
                }
            }
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

    /// Add `files` to `command`, through a response file in `rsp/<name>.rsp` when the
    /// list is long.
    fn add_files(&self, command: &mut Command, files: &[PathBuf], name: &str) -> Result<()> {
        let length: usize = files.iter().map(|f| f.as_os_str().len() + 1).sum();
        if length <= RESPONSE_FILE_THRESHOLD {
            command.args(files);
            return Ok(());
        }
        let dir = self.out.join("rsp");
        create_dir(&dir)?;
        let path = dir.join(format!("{name}.rsp"));
        let mut text = String::with_capacity(length * 2);
        for file in files {
            text.push_str(&response_file_arg(file));
            text.push('\n');
        }
        fs::write(&path, text).with_context(|| format!("failed to write `{}`", path.display()))?;
        command.arg(format!("@{}", path.display()));
        Ok(())
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

/// `<dir>/<source>.o`, creating its directory.
fn object_path(dir: &Path, source: &str) -> Result<PathBuf> {
    let path = dir.join(format!("{source}.o"));
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    Ok(path)
}

/// The file Clang looks for in a `-fprebuilt-module-path` directory: `m.pcm`, or
/// `m-part.pcm` for the partition `m:part`.
fn pcm_name(module: &str) -> String {
    format!("{}.pcm", module.replace(':', "-"))
}

/// A path as one quoted response-file argument. Forward slashes on Windows, so the
/// GNU and Windows response-file rules read it the same way.
fn response_file_arg(path: &Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        text = text.replace('\\', "/");
    }
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn could_not(verb: &str, target: &Target) -> String {
    format!(
        "could not {verb} `{}` ({})",
        target.name,
        kind_name(target.kind)
    )
}

/// The directory holding libc++ itself: the one with `libc++.modules.json` in it.
fn libcxx_dir(toolchain: &Toolchain) -> Option<PathBuf> {
    let manifest = &toolchain.std_modules.manifest;
    fs::canonicalize(manifest)
        .unwrap_or_else(|_| manifest.clone())
        .parent()
        .map(Path::to_path_buf)
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
    fn multi_file_module_library_with_partitions_and_two_bins() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("shapes");
        write(
            &dir.join("src/lib.cppm"),
            "export module shapes;\nexport import :area;\nexport import shapes.units;\n\nexport namespace shapes {\nint scaled(int x);\n}\n",
        );
        write(
            &dir.join("src/area.cppm"),
            "export module shapes:area;\nimport shapes.units;\n\nexport namespace shapes {\nint square(int side) { return side * side * units::kScale; }\n}\n",
        );
        // An internal partition in a `.cpp` file, and a module interface in one.
        write(
            &dir.join("src/detail.cpp"),
            "module shapes:detail;\n\nnamespace shapes::detail {\nint twice(int x) { return 2 * x; }\n}\n",
        );
        write(
            &dir.join("src/units/units.cpp"),
            "export module shapes.units;\n\nexport namespace shapes::units {\ninline constexpr int kScale = 1;\n}\n",
        );
        write(
            &dir.join("src/lib.cpp"),
            "module shapes;\nimport :detail;\n\nnamespace shapes {\nint scaled(int x) { return detail::twice(x); }\n}\n",
        );
        write(
            &dir.join("src/main.cpp"),
            "import std;\nimport shapes;\n#include \"helper.hpp\"\n\nint main() {\n    std::println(\"{} {}\", shapes::square(3), helper());\n}\n",
        );
        write(&dir.join("src/helper.hpp"), "int helper();\n");
        write(
            &dir.join("src/helper.cpp"),
            "import shapes;\n#include \"helper.hpp\"\n\nint helper() { return shapes::scaled(21); }\n",
        );
        write(
            &dir.join("Yam.toml"),
            "[project]\nname = \"shapes\"\n\n[lib]\nexclude = [\"src/helper.cpp\"]\n\n[[bin]]\nname = \"shapes\"\nsources = [\"src/main.cpp\", \"src/helper.cpp\"]\n",
        );
        write(
            &dir.join("src/bin/tool.cpp"),
            "import std;\nimport shapes;\n\nint main() { std::println(\"{}\", shapes::square(5)); }\n",
        );
        run(&build_args(&dir)).unwrap();
        assert_eq!(run_binary(&exe(&dir, "Debug", "shapes")), "9 42\n");
        assert_eq!(run_binary(&exe(&dir, "Debug", "tool")), "25\n");
        let modules = dir.join("target/Debug/modules/shapes");
        for pcm in [
            "shapes.pcm",
            "shapes-area.pcm",
            "shapes-detail.pcm",
            "shapes.units.pcm",
        ] {
            assert!(modules.join(pcm).is_file(), "{pcm}");
        }
        assert!(
            dir.join("target/Debug/obj/shapes/bin/shapes/src/helper.cpp.o")
                .is_file()
        );
    }

    #[test]
    fn legacy_targets_use_include_dirs_and_link_vendor_libraries() {
        if !have_toolchain() {
            return;
        }
        let toolchain = Toolchain::discover().unwrap();
        let temp = tempdir().unwrap();
        let dir = temp.path().join("port");

        // A prebuilt vendor library, as a closed-source dependency would arrive.
        write(
            &dir.join("vendor/acme.cpp"),
            "int acme_answer() { return 40; }\n",
        );
        let vendor = dir.join("vendor");
        let status = Command::new(&toolchain.cxx.path)
            .args(["-stdlib=libc++", "-c"])
            .arg(vendor.join("acme.cpp"))
            .arg("-o")
            .arg(vendor.join("acme.o"))
            .status()
            .unwrap();
        assert!(status.success());
        for archive in ["libacme.a", "libacme2.a"] {
            let status = Command::new(&toolchain.ar)
                .arg("rcs")
                .arg(vendor.join(archive))
                .arg(vendor.join("acme.o"))
                .status()
                .unwrap();
            assert!(status.success());
        }
        fs::remove_file(vendor.join("acme.cpp")).unwrap();

        write(
            &dir.join("Yam.toml"),
            "[project]\nname = \"port\"\nstd = \"c++17\"\n\n[lib]\npath = \"lib/port.cpp\"\nsources = [\"lib/**/*.cpp\", \"lib/**/*.hpp\"]\nexclude = [\"lib/test/**\"]\ninclude-dirs = [\"include\"]\nlinks = [\"vendor/libacme.a\"]\n\n[[bin]]\nname = \"port-cli\"\npath = \"cli/main.cpp\"\nsources = [\"cli/*.cpp\"]\nlinks = [\"acme2\"]\nlink-dirs = [\"vendor\"]\n",
        );
        write(&dir.join("include/port/port.hpp"), "int port_value();\n");
        write(
            &dir.join("lib/port.cpp"),
            "#include <port/port.hpp>\n#include \"detail/two.hpp\"\nint acme_answer();\nint port_value() { return acme_answer() + two(); }\n",
        );
        write(&dir.join("lib/detail/two.hpp"), "int two();\n");
        write(&dir.join("lib/detail/two.cpp"), "int two() { return 2; }\n");
        write(&dir.join("lib/test/broken.cpp"), "this is not C++\n");
        write(
            &dir.join("cli/main.cpp"),
            "#include <cstdio>\n#include <port/port.hpp>\nint acme_answer();\nint main() { std::printf(\"%d %d\\n\", port_value(), acme_answer()); }\n",
        );
        run(&build_args(&dir)).unwrap();
        assert_eq!(run_binary(&exe(&dir, "Debug", "port-cli")), "42 40\n");
        // No `std.pcm` without `import std`.
        assert!(!dir.join("target/Debug/std").exists());
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

    /// Mistakes found before any compiler runs, so these need no toolchain.
    fn build_error(manifest: &str, files: &[(&str, &str)]) -> String {
        let temp = tempdir().unwrap();
        write(&temp.path().join("Yam.toml"), manifest);
        for (path, text) in files {
            write(&temp.path().join(path), text);
        }
        run(&build_args(temp.path())).unwrap_err().to_string()
    }

    #[test]
    fn two_mains_in_one_bin_is_an_error() {
        let err = build_error(
            "[project]\nname = \"app\"\n\n[[bin]]\nname = \"app\"\nsources = [\"src/*.cpp\"]\n",
            &[
                ("src/main.cpp", "int main() {}\n"),
                ("src/old_main.cpp", "int main() { return 1; }\n"),
            ],
        );
        assert!(
            err.starts_with(
                "bin target `app` has more than one `main()`: `src/main.cpp`, `src/old_main.cpp`"
            ),
            "{err}"
        );
    }

    #[test]
    fn import_std_needs_cxx23() {
        let err = build_error(
            "[project]\nname = \"app\"\nstd = \"c++20\"\n",
            &[("src/main.cpp", "import std;\nint main() {}\n")],
        );
        assert!(
            err.starts_with(
                "`src/main.cpp` imports `std`, which needs c++23 or later, but the project's std is c++20"
            ),
            "{err}"
        );
    }

    #[test]
    fn bins_can_only_import_the_library_they_link() {
        let err = build_error(
            "[project]\nname = \"app\"\n",
            &[("src/main.cpp", "import nowhere;\nint main() {}\n")],
        );
        assert!(
            err.starts_with(
                "`src/main.cpp` imports module `nowhere`, but no source file of bin target `app` declares it"
            ),
            "{err}"
        );
    }

    #[test]
    fn a_target_needs_a_source_file() {
        let err = build_error(
            "[project]\nname = \"app\"\n\n[lib]\npath = \"include/app.hpp\"\nsources = [\"include/*.hpp\"]\n",
            &[("include/app.hpp", "int f();\n")],
        );
        assert_eq!(err, "lib target `app` has no source files");
    }

    fn target(name: &str, kind: TargetKind, links_lib: bool) -> Target {
        Target {
            name: name.into(),
            kind,
            entry: PathBuf::new(),
            sources: Vec::new(),
            exclude: Vec::new(),
            include_dirs: Vec::new(),
            links: Vec::new(),
            link_dirs: Vec::new(),
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
    fn names_compiled_interfaces_the_way_clang_looks_them_up() {
        assert_eq!(pcm_name("calc"), "calc.pcm");
        assert_eq!(pcm_name("a.b:part"), "a.b-part.pcm");
    }

    #[test]
    fn quotes_response_file_arguments() {
        assert_eq!(
            response_file_arg(Path::new("dir with space/a\"b.o")),
            "\"dir with space/a\\\"b.o\""
        );
    }

    #[test]
    fn displays_commands() {
        let mut command = Command::new("clang++");
        command.args(["-c", "a b.cpp", ""]);
        assert_eq!(display(&command), "clang++ -c \"a b.cpp\" \"\"");
    }
}
