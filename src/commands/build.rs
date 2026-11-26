//! `yam build`: compile every file of the project's lib and bins, archive the lib and
//! link the bins.
//!
//! One batched `clang-scan-deps` call finds the module dependencies of every new or
//! changed file ([`engine::scan`], cached by [`engine::ScanCache`]), each target's
//! files are put in compile order ([`engine::plan`]), and the build graph
//! ([`engine::graph`]) is run one node at a time. A node whose output is fresh, by the
//! output record ([`engine::Outputs`]), is skipped, so a no-op build compiles
//! nothing. The parallel scheduler (#25) runs independent nodes at the same time.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::cli::BuildArgs;
use crate::engine::{
    self, Action, CommandHash, Deps, FlagsHash, GraphTarget, HeaderRecord, Key, Outputs, Plan,
    STD_MODULE, ScanCache, ScanJob, SourceFile,
};
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
/// files, a bin has two `main()`s, the toolchain can't be found, scanning fails, a
/// target's modules can't be ordered, or a compile, archive or link step fails.
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
    let selected = select_files(&root, &targets, &tree)?;

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
    let out = root.join("target").join(profile.dir_name());
    let builder = Builder {
        root: &root,
        project: &manifest.project.name,
        std: manifest.project.std,
        profile,
        toolchain: &toolchain,
        verbose: args.verbose,
        caches: RefCell::new(Caches::load(&out)),
        out,
    };
    // Whatever the build got through is kept, even when a step fails.
    let built = build(&builder, selected);
    builder.caches.borrow_mut().save();
    built?;

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

/// Scan and order the selected targets, then run the build graph, skipping every
/// node whose output is fresh.
fn build(builder: &Builder<'_>, selected: Vec<Selected<'_>>) -> Result<()> {
    let prepared = builder.prepare(selected)?;
    let graph_targets: Vec<GraphTarget<'_>> = prepared
        .iter()
        .map(|p| GraphTarget {
            is_lib: p.target.kind == TargetKind::Lib,
            links_lib: p.target.links_lib,
            files: &p.files,
            plan: &p.plan,
        })
        .collect();
    let nodes = engine::graph(&graph_targets);
    let lib = prepared
        .iter()
        .find(|p| p.target.kind == TargetKind::Lib)
        .map(|p| builder.lib(p));

    let mut std_module: Option<StdModule> = None;
    // The compiled module interfaces each node reads, directly or through the
    // modules it imports.
    let mut pcms: Vec<Vec<PathBuf>> = vec![Vec::new(); nodes.len()];
    let (mut ran, mut compiles) = (0, 0);
    for (id, node) in nodes.iter().enumerate() {
        let mut reads = std::collections::BTreeSet::new();
        for &need in &node.needs {
            match nodes[need].action {
                Action::Std => {
                    if let Some(std) = &std_module {
                        reads.insert(std.pcm.clone());
                    }
                }
                Action::Compile { target, step } => {
                    if let Some(pcm) = builder.pcm_path(&prepared[target], step) {
                        reads.insert(pcm);
                    }
                    reads.extend(pcms[need].iter().cloned());
                }
                Action::Archive { .. } | Action::Link { .. } => {}
            }
        }
        pcms[id] = reads.into_iter().collect();

        match node.action {
            Action::Std => std_module = Some(builder.std_module()?),
            Action::Compile { target, step } => {
                let prepared = &prepared[target];
                let lib = lib.as_ref().filter(|_| {
                    prepared.target.kind == TargetKind::Bin && prepared.target.links_lib
                });
                compiles += 1;
                if builder.compile(prepared, step, std_module.as_ref(), lib, &pcms[id])? {
                    ran += 1;
                }
            }
            Action::Archive { target } => builder.archive(&prepared[target])?,
            Action::Link { target } => {
                let prepared = &prepared[target];
                let lib = lib.as_ref().filter(|_| prepared.target.links_lib);
                builder.link(prepared, std_module.as_ref(), lib)?;
            }
        }
    }
    if builder.verbose {
        style::status("Compiled", format!("{ran} of {compiles} files"));
    }
    Ok(())
}

/// What one build hands the next: scan results and the headers each object was
/// compiled with. Loaded once, and saved at the end even if the build fails.
struct Caches {
    scan: ScanCache,
    headers: HeaderRecord,
    outputs: Outputs,
    /// Header contents hashes for this build, so each header is read once.
    header_hashes: HashMap<PathBuf, Option<blake3::Hash>>,
}

impl Caches {
    fn load(out: &Path) -> Self {
        let dir = out.join(".yam");
        let (scan, discarded) = ScanCache::load(&dir.join("scan-cache.json"));
        if let Some(reason) = discarded {
            style::note(format!("{reason}; scanning every file again"));
        }
        let (headers, discarded) = HeaderRecord::load(&dir.join("headers.json"));
        if let Some(reason) = discarded {
            style::note(format!("{reason}; header dependencies start over"));
        }
        let (outputs, discarded) = Outputs::load(&dir.join("outputs.json"));
        if let Some(reason) = discarded {
            style::note(format!("{reason}; building everything again"));
        }
        Self {
            scan,
            headers,
            outputs,
            header_hashes: HashMap::new(),
        }
    }

    /// The scan key for a file, using the headers `object` was last compiled with.
    fn key(&mut self, root: &Path, file: &FileKey, object: &Path) -> Key {
        let headers = self.headers.get(object).to_vec();
        self.key_with(root, file, &headers)
    }

    /// The scan key for a file compiled with `headers`.
    fn key_with(&mut self, root: &Path, file: &FileKey, headers: &[String]) -> Key {
        let hashes: Vec<Option<blake3::Hash>> = headers
            .iter()
            .map(|header| {
                let path = root.join(header);
                *self.header_hashes.entry(path).or_insert_with_key(|path| {
                    fs::read(path).ok().map(|bytes| blake3::hash(&bytes))
                })
            })
            .collect();
        let pairs: Vec<engine::HeaderHash<'_>> =
            headers.iter().map(String::as_str).zip(hashes).collect();
        Key::new(&file.flags, &file.contents, &pairs)
    }

    fn save(&mut self) {
        if let Err(err) = self.scan.save_if_changed() {
            style::warning(format!("couldn't save the scan cache: {err}"));
        }
        if let Err(err) = self.headers.save_if_changed() {
            style::warning(format!("couldn't save the header record: {err}"));
        }
        if let Err(err) = self.outputs.save_if_changed() {
            style::warning(format!("couldn't save the output record: {err}"));
        }
    }
}

/// What a file's scan key is made of, apart from its headers.
struct FileKey {
    flags: FlagsHash,
    contents: blake3::Hash,
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

/// A target and the files it compiles, relative to the root and sorted.
struct Selected<'t> {
    target: &'t Target,
    files: Vec<String>,
}

/// A target with its files scanned and put in compile order.
struct Prepared<'t> {
    target: &'t Target,
    files: Vec<SourceFile>,
    /// Each file's scan key parts, in the same order as `files`.
    keys: Vec<FileKey>,
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

/// Every target's files, checking what can be checked without a toolchain: each
/// target has a source file, and each bin has one `main()`.
fn select_files<'t>(
    root: &Path,
    targets: &'t [Target],
    tree: &SourceTree,
) -> Result<Vec<Selected<'t>>> {
    let mut selected = Vec::with_capacity(targets.len());
    for target in targets {
        let files = source_files(target, tree)?;
        if target.kind == TargetKind::Bin {
            let mut mains = Vec::new();
            for file in &files {
                let text = fs::read_to_string(root.join(file))
                    .with_context(|| format!("failed to read `{file}`"))?;
                if engine::defines_main(&text) {
                    mains.push(file.clone());
                }
            }
            engine::check_one_main(&describe(target), mains)?;
        }
        selected.push(Selected { target, files });
    }
    Ok(selected)
}

/// The target's files to compile, sorted by path.
fn source_files(target: &Target, tree: &SourceTree) -> Result<Vec<String>> {
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
        if SOURCE_EXTENSIONS.contains(&extension) {
            files.push(path.to_string());
        } else if !HEADER_EXTENSIONS.contains(&extension) {
            style::warning(format!(
                "skipping `{path}` in {}: not a C++ source file",
                describe(target)
            ));
        }
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
    caches: RefCell<Caches>,
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

    /// Scan every selected file in one `clang-scan-deps` call, then order each
    /// target's files. A bin that links the library may import its modules.
    fn prepare<'t>(&self, selected: Vec<Selected<'t>>) -> Result<Vec<Prepared<'t>>> {
        let lib_target = selected
            .iter()
            .map(|s| s.target)
            .find(|t| t.kind == TargetKind::Lib);
        let scanned = self.scan(&selected, lib_target)?;
        let mut scanned = scanned.into_iter();

        let mut prepared: Vec<Prepared<'t>> = Vec::with_capacity(selected.len());
        let mut lib_modules = HashSet::new();
        let none = HashSet::new();
        for Selected { target, files } in selected {
            let (files, keys): (Vec<SourceFile>, Vec<FileKey>) = files
                .into_iter()
                .zip(scanned.by_ref())
                .map(|(path, (deps, key))| (SourceFile { path, deps }, key))
                .unzip();
            let available = if target.kind == TargetKind::Bin && target.links_lib {
                &lib_modules
            } else {
                &none
            };
            let plan = engine::plan(&describe(target), &files, available)?;
            if plan.imports_std && !self.std.supports_import_std() {
                let file = files
                    .iter()
                    .find(|f| f.deps.requires.iter().any(|m| m == STD_MODULE))
                    .map_or("", |f| f.path.as_str());
                bail!(
                    "`{file}` imports `std`, which needs c++23 or later, but the project's \
                     std is {}\n\nhelp: set `std = \"c++23\"` or later in [project]",
                    self.std.as_str()
                );
            }
            let target = Prepared {
                target,
                files,
                keys,
                plan,
            };
            if target.target.kind == TargetKind::Lib {
                lib_modules = target.modules();
            }
            prepared.push(target);
        }
        Ok(prepared)
    }

    /// Every selected file's module dependencies and scan key, in order: from the
    /// scan cache when its contents, headers and flags are unchanged, otherwise from
    /// one `clang-scan-deps` run over the rest. A build where nothing changed starts
    /// no scanner at all.
    fn scan(
        &self,
        selected: &[Selected<'_>],
        lib_target: Option<&Target>,
    ) -> Result<Vec<(Deps, FileKey)>> {
        let toolchain = self.toolchain;
        let tools = format!(
            "{} {} {} {}",
            toolchain.scan_deps.path.display(),
            toolchain.scan_deps.version,
            toolchain.cxx.path.display(),
            toolchain.cxx.version
        );
        let mut caches = self.caches.borrow_mut();

        let mut results: Vec<(Option<Deps>, FileKey)> = Vec::new();
        let mut misses = Vec::new();
        let mut jobs = Vec::new();
        for Selected { target, files } in selected {
            let (obj_dir, _) = self.target_dirs(target);
            let mut command = self.compile_command();
            for dir in self.include_dirs(target, lib_target) {
                command.arg(format!("-I{}", dir.display()));
            }
            let flags = FlagsHash::new(
                &tools,
                std::iter::once(command.get_program()).chain(command.get_args()),
            );
            for file in files {
                let source = self.root.join(file);
                let contents =
                    fs::read(&source).with_context(|| format!("failed to read `{file}`"))?;
                let file_key = FileKey {
                    flags: flags.clone(),
                    contents: blake3::hash(&contents),
                };
                let output = object_file(&obj_dir, file);
                let key = caches.key(self.root, &file_key, &output);
                if let Some(deps) = caches.scan.get(key) {
                    results.push((Some(deps), file_key));
                    continue;
                }
                create_parent(&output)?;
                let arguments = std::iter::once(command.get_program())
                    .chain(command.get_args())
                    .map(ToOwned::to_owned)
                    .chain([
                        "-c".into(),
                        source.clone().into_os_string(),
                        "-o".into(),
                        output.clone().into_os_string(),
                    ])
                    .collect();
                misses.push((results.len(), key));
                results.push((None, file_key));
                jobs.push(ScanJob {
                    source,
                    output,
                    arguments,
                });
            }
        }

        if self.verbose {
            style::status(
                "Scanning",
                format!(
                    "{} of {} files ({} cached)",
                    jobs.len(),
                    results.len(),
                    results.len() - jobs.len()
                ),
            );
        }
        if !jobs.is_empty() {
            let database = self.out.join("scan").join("compile_commands.json");
            let fresh = engine::scan(&toolchain.scan_deps.path, &database, &jobs, self.verbose)?;
            for ((index, key), deps) in misses.into_iter().zip(fresh) {
                caches.scan.insert(key, deps.clone());
                results[index].0 = Some(deps);
            }
        }
        Ok(results
            .into_iter()
            .filter_map(|(deps, key)| Some((deps?, key)))
            .collect())
    }

    /// A target's header search directories, absolute: its own, then the library's
    /// for a bin that links it.
    fn include_dirs(&self, target: &Target, lib: Option<&Target>) -> Vec<PathBuf> {
        let lib = lib.filter(|_| target.kind == TargetKind::Bin && target.links_lib);
        target
            .include_dirs
            .iter()
            .chain(lib.into_iter().flat_map(|lib| &lib.include_dirs))
            .map(|dir| self.root.join(dir))
            .collect()
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

    /// The compiler's identity, part of every command hash.
    fn tools(&self) -> String {
        format!(
            "{} {}",
            self.toolchain.cxx.path.display(),
            self.toolchain.cxx.version
        )
    }

    /// The object a target's plan step compiles to.
    fn object(&self, prepared: &Prepared<'_>, step: usize) -> PathBuf {
        let (obj_dir, _) = self.target_dirs(prepared.target);
        let file = prepared.plan.steps[step].file;
        object_file(&obj_dir, &prepared.files[file].path)
    }

    /// The compiled interface a target's plan step writes, if its file provides a
    /// module.
    fn pcm_path(&self, prepared: &Prepared<'_>, step: usize) -> Option<PathBuf> {
        let (_, module_dir) = self.target_dirs(prepared.target);
        let module = prepared.plan.steps[step].provides.as_ref()?;
        Some(module_dir.join(pcm_name(module)))
    }

    /// Compile one file, unless its object (and interface) are fresh. `pcms` are the
    /// compiled interfaces it reads. Returns whether it ran.
    fn compile(
        &self,
        prepared: &Prepared<'_>,
        step: usize,
        std: Option<&StdModule>,
        lib: Option<&Lib>,
        pcms: &[PathBuf],
    ) -> Result<bool> {
        let target = prepared.target;
        let (_, module_dir) = self.target_dirs(target);
        let file = prepared.plan.steps[step].file;
        let source = self.root.join(&prepared.files[file].path);
        let object = self.object(prepared, step);
        let pcm = self.pcm_path(prepared, step);
        let depfile = depfile_path(&object);

        let mut include_dirs: Vec<PathBuf> = target
            .include_dirs
            .iter()
            .map(|dir| self.root.join(dir))
            .collect();
        let mut module_dirs = Vec::new();
        if prepared.plan.steps.iter().any(|s| s.provides.is_some()) {
            module_dirs.push(module_dir);
        }
        if let Some(lib) = lib {
            include_dirs.extend(lib.include_dirs.iter().cloned());
            module_dirs.extend(lib.module_dir.iter().cloned());
        }

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
        if let Some(pcm) = &pcm {
            compile.arg(format!("-fmodule-output={}", pcm.display()));
            // `.cppm` is a module interface by its extension; anything else that
            // declares a module needs saying so.
            if !engine::has_extension(&prepared.files[file].path, "cppm") {
                compile.args(["-x", "c++-module"]);
            }
        }
        compile
            .arg("-MMD")
            .arg("-MF")
            .arg(&depfile)
            .arg("-c")
            .arg(&source)
            .arg("-o")
            .arg(&object);
        let command = command_hash(&self.tools(), &compile);

        let headers = self.caches.borrow().headers.get(&object).to_vec();
        let inputs = self.compile_inputs(&source, &headers, pcms);
        let fresh = self
            .caches
            .borrow_mut()
            .outputs
            .is_fresh(&object, command, &inputs);
        if fresh && pcm.as_ref().is_none_or(|pcm| pcm.exists()) {
            return Ok(false);
        }

        create_parent(&object)?;
        if let Some(pcm) = &pcm {
            create_parent(pcm)?;
        }
        self.run(&mut compile, &could_not("compile", target))?;
        let headers = self.record_headers(prepared, file, &object, &depfile);
        let inputs = self.compile_inputs(&source, &headers, pcms);
        let mut caches = self.caches.borrow_mut();
        caches.outputs.record(&object, command, &inputs);
        caches.outputs.invalidate(&object);
        if let Some(pcm) = &pcm {
            caches.outputs.invalidate(pcm);
        }
        Ok(true)
    }

    /// What a compile reads: the source, its project headers and the compiled
    /// interfaces it imports.
    fn compile_inputs(&self, source: &Path, headers: &[String], pcms: &[PathBuf]) -> Vec<PathBuf> {
        std::iter::once(source.to_path_buf())
            .chain(headers.iter().map(|header| self.root.join(header)))
            .chain(pcms.iter().cloned())
            .collect()
    }

    /// Read a compiled file's depfile into the header record, and file its scan
    /// result under the key for those headers, so the next build finds it. A missing
    /// or unreadable depfile records no headers. Returns the headers.
    fn record_headers(
        &self,
        prepared: &Prepared<'_>,
        file: usize,
        object: &Path,
        depfile: &Path,
    ) -> Vec<String> {
        let source = self.root.join(&prepared.files[file].path);
        let deps = fs::read_to_string(depfile).map_or_default(|text| engine::depfile::parse(&text));
        let headers = engine::headers_from_depfile(deps, self.root, &source);
        let mut caches = self.caches.borrow_mut();
        let key = caches.key_with(self.root, &prepared.keys[file], &headers);
        caches.scan.insert(key, prepared.files[file].deps.clone());
        caches.headers.record(object, headers.clone());
        headers
    }

    /// The library as a bin uses it. Nothing is built here.
    fn lib(&self, prepared: &Prepared<'_>) -> Lib {
        let target = prepared.target;
        let (_, module_dir) = self.target_dirs(target);
        let has_modules = prepared.plan.steps.iter().any(|s| s.provides.is_some());
        Lib {
            archive: self.out.join(format!("lib{}.a", target.name)),
            module_dir: has_modules.then_some(module_dir),
            include_dirs: target
                .include_dirs
                .iter()
                .map(|dir| self.root.join(dir))
                .collect(),
            links: target.links.clone(),
            link_dirs: target.link_dirs.clone(),
        }
    }

    /// A target's objects, in plan order.
    fn objects(&self, prepared: &Prepared<'_>) -> Vec<PathBuf> {
        (0..prepared.plan.steps.len())
            .map(|step| self.object(prepared, step))
            .collect()
    }

    /// Archive the library's objects, unless the archive is fresh.
    fn archive(&self, prepared: &Prepared<'_>) -> Result<()> {
        let target = prepared.target;
        let objects = self.objects(prepared);
        let archive = self.out.join(format!("lib{}.a", target.name));
        let before = [OsString::from("rcs"), archive.clone().into_os_string()];
        let command = CommandHash::new(
            &self.toolchain.ar.display().to_string(),
            before
                .iter()
                .map(OsString::as_os_str)
                .chain(objects.iter().map(|o| o.as_os_str())),
        );
        if self
            .caches
            .borrow_mut()
            .outputs
            .is_fresh(&archive, command, &objects)
        {
            return Ok(());
        }

        let _ = fs::remove_file(&archive);
        let mut ar = Command::new(&self.toolchain.ar);
        ar.args(&before);
        self.add_files(&mut ar, &objects, &format!("lib{}.a", target.name))?;
        self.run(&mut ar, &could_not("archive", target))?;
        let mut caches = self.caches.borrow_mut();
        caches.outputs.record(&archive, command, &objects);
        caches.outputs.invalidate(&archive);
        Ok(())
    }

    /// Link a bin, unless its executable is fresh.
    fn link(
        &self,
        prepared: &Prepared<'_>,
        std: Option<&StdModule>,
        lib: Option<&Lib>,
    ) -> Result<()> {
        let target = prepared.target;
        let mut objects = self.objects(prepared);
        if let Some(std) = std {
            objects.push(std.object.clone());
        }
        let executable = self
            .out
            .join(format!("{}{}", target.name, std::env::consts::EXE_SUFFIX));

        // Everything after the objects; library files are inputs too.
        let mut after: Vec<OsString> = Vec::new();
        let mut inputs = objects.clone();
        if let Some(lib) = lib {
            after.push(lib.archive.clone().into_os_string());
            inputs.push(lib.archive.clone());
        }
        // The bin's own libraries, then the project library's, which its archive
        // needs but can't carry.
        let lib_links = lib.map(|lib| (&lib.links, &lib.link_dirs));
        for (links, link_dirs) in
            std::iter::once((&target.links, &target.link_dirs)).chain(lib_links)
        {
            for dir in link_dirs {
                after.push(format!("-L{}", self.root.join(dir).display()).into());
            }
            for name in links {
                if is_link_path(name) {
                    let path = self.root.join(name);
                    after.push(path.clone().into_os_string());
                    inputs.push(path);
                } else {
                    after.push(format!("-l{name}").into());
                }
            }
        }
        // Link against the libc++ that matches the headers, not whichever one the
        // linker finds first (Homebrew's lives outside the default search path).
        if let Some(dir) = libcxx_dir(self.toolchain) {
            after.push(format!("-L{}", dir.display()).into());
            if cfg!(target_os = "macos") {
                after.push(format!("-Wl,-rpath,{}", dir.display()).into());
            }
        }
        after.push("-o".into());
        after.push(executable.clone().into_os_string());

        let before = OsString::from("-stdlib=libc++");
        let command = CommandHash::new(
            &self.tools(),
            std::iter::once(self.toolchain.cxx.path.as_os_str())
                .chain([before.as_os_str()])
                .chain(objects.iter().map(|o| o.as_os_str()))
                .chain(after.iter().map(OsString::as_os_str)),
        );
        if self
            .caches
            .borrow_mut()
            .outputs
            .is_fresh(&executable, command, &inputs)
        {
            return Ok(());
        }

        let mut link = Command::new(&self.toolchain.cxx.path);
        link.arg(&before);
        self.add_files(&mut link, &objects, &target.name)?;
        link.args(&after);
        self.run(&mut link, &could_not("link", target))?;
        let mut caches = self.caches.borrow_mut();
        caches.outputs.record(&executable, command, &inputs);
        caches.outputs.invalidate(&executable);
        Ok(())
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

/// A hash of `command` as it will run, with `tools` identifying the compiler.
fn command_hash(tools: &str, command: &Command) -> CommandHash {
    CommandHash::new(
        tools,
        std::iter::once(command.get_program()).chain(command.get_args()),
    )
}

/// `<dir>/<source>.o`.
fn object_file(dir: &Path, source: &str) -> PathBuf {
    dir.join(format!("{source}.o"))
}

fn create_parent(path: &Path) -> Result<()> {
    match path.parent() {
        Some(parent) => create_dir(parent),
        None => Ok(()),
    }
}

/// The depfile next to an object: `<object>.d`.
fn depfile_path(object: &Path) -> PathBuf {
    let mut path = object.as_os_str().to_owned();
    path.push(".d");
    PathBuf::from(path)
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

    /// How many files the last scan covered: the entries in its compilation database,
    /// which is written only when something needs scanning.
    fn scanned(dir: &Path) -> Option<usize> {
        let path = dir.join("target/Debug/scan/compile_commands.json");
        let text = fs::read_to_string(&path).ok()?;
        fs::remove_file(&path).unwrap();
        let entries: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        Some(entries.len())
    }

    #[test]
    fn unchanged_files_are_never_rescanned() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("cached");
        new_project(&dir, true, false);
        write(
            &dir.join("src/main.cpp"),
            "import std;\nimport cached;\n\nint main() {\n    std::println(\"{}\", cached::add(1, 2));\n}\n",
        );
        let cache = dir.join("target/Debug/.yam/scan-cache.json");

        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), Some(2));
        let saved = fs::metadata(&cache).unwrap().modified().unwrap();

        // A no-op build scans nothing and leaves the cache file alone.
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), None);
        assert_eq!(fs::metadata(&cache).unwrap().modified().unwrap(), saved);

        // Rewriting a file with the same contents is still a hit.
        let main = fs::read_to_string(dir.join("src/main.cpp")).unwrap();
        write(&dir.join("src/main.cpp"), &main);
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), None);

        // Only the edited file is scanned again.
        write(&dir.join("src/main.cpp"), &main.replace("(1, 2)", "(2, 2)"));
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), Some(1));
        assert_eq!(run_binary(&exe(&dir, "Debug", "cached")), "4\n");

        // A release build has its own cache.
        let mut release = build_args(&dir);
        release.release = true;
        run(&release).unwrap();
        assert!(dir.join("target/Release/.yam/scan-cache.json").is_file());

        // A corrupt cache is discarded, and everything is scanned again.
        fs::write(&cache, "not json").unwrap();
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), Some(2));
    }

    #[test]
    fn a_header_change_rescans_the_files_that_include_it() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("flag");
        write(&dir.join("Yam.toml"), "[project]\nname = \"flag\"\n");
        write(&dir.join("src/config.h"), "#define USE_STD 0\n");
        write(
            &dir.join("src/main.cpp"),
            "#include \"config.h\"\n#if USE_STD\nimport std;\n#endif\n#include <cstdio>\n\nint main() {\n#if USE_STD\n    std::println(\"std\");\n#else\n    std::printf(\"plain\\n\");\n#endif\n}\n",
        );
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), Some(1));
        assert_eq!(run_binary(&exe(&dir, "Debug", "flag")), "plain\n");
        assert!(!dir.join("target/Debug/std").exists());
        let record = fs::read_to_string(dir.join("target/Debug/.yam/headers.json")).unwrap();
        assert!(record.contains("src/config.h"), "{record}");
        assert!(
            !record.contains("c++/v1"),
            "toolchain headers aren't recorded: {record}"
        );

        // The source didn't change, but its header did, so it's scanned again and
        // now needs `std.pcm`.
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), None);
        write(&dir.join("src/config.h"), "#define USE_STD 1\n");
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), Some(1));
        assert_eq!(run_binary(&exe(&dir, "Debug", "flag")), "std\n");
        assert!(dir.join("target/Debug/std/c++26/std.pcm").is_file());

        // And settles again.
        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), None);
    }

    /// The modification time of every object, archive and executable a Debug build
    /// left, by path relative to `target/Debug`.
    fn outputs(dir: &Path) -> std::collections::BTreeMap<String, std::time::SystemTime> {
        let debug = dir.join("target/Debug");
        walkdir::WalkDir::new(&debug)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy();
                name.ends_with(".o")
                    || name.ends_with(".a")
                    || name.ends_with(".pcm")
                    || entry.depth() == 1 && !name.contains('.')
                    || name.ends_with(".exe")
            })
            .map(|entry| {
                let path = entry.path().strip_prefix(&debug).unwrap();
                let modified = entry.metadata().unwrap().modified().unwrap();
                (manifest::slash_path(path), modified)
            })
            .collect()
    }

    /// The outputs whose mtime differs between two snapshots.
    fn rebuilt(
        before: &std::collections::BTreeMap<String, std::time::SystemTime>,
        after: &std::collections::BTreeMap<String, std::time::SystemTime>,
    ) -> Vec<String> {
        after
            .iter()
            .filter(|(path, time)| before.get(*path) != Some(time))
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Set a file's mtime to now plus `seconds`, without changing its contents.
    fn touch(path: &Path, seconds: u64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(seconds))
            .unwrap();
    }

    #[test]
    fn only_stale_outputs_are_rebuilt() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("calc");
        new_project(&dir, true, false);
        write(
            &dir.join("src/lib.cpp"),
            "module calc;\n\nnamespace calc {\nint twice(int x) { return 2 * x; }\n}\n",
        );
        let interface = fs::read_to_string(dir.join("src/lib.cppm")).unwrap();
        write(
            &dir.join("src/lib.cppm"),
            &format!("{interface}\nexport namespace calc {{\nint twice(int x);\n}}\n"),
        );
        write(
            &dir.join("src/main.cpp"),
            "import std;\nimport calc;\n\nint main() {\n    std::println(\"{}\", calc::twice(calc::add(1, 2)));\n}\n",
        );
        run(&build_args(&dir)).unwrap();
        assert_eq!(run_binary(&exe(&dir, "Debug", "calc")), "6\n");
        let exe_name = format!("calc{}", std::env::consts::EXE_SUFFIX);

        // A no-op build, and one after `touch`, rewrite nothing.
        let first = outputs(&dir);
        run(&build_args(&dir)).unwrap();
        assert_eq!(rebuilt(&first, &outputs(&dir)), Vec::<String>::new());
        touch(&dir.join("src/lib.cppm"), 5);
        touch(&dir.join("src/main.cpp"), 5);
        run(&build_args(&dir)).unwrap();
        assert_eq!(rebuilt(&first, &outputs(&dir)), Vec::<String>::new());

        // An implementation change recompiles that file, re-archives and relinks.
        write(
            &dir.join("src/lib.cpp"),
            "module calc;\n\nnamespace calc {\nint twice(int x) { return x + x + 1; }\n}\n",
        );
        let before = outputs(&dir);
        run(&build_args(&dir)).unwrap();
        assert_eq!(
            rebuilt(&before, &outputs(&dir)),
            [
                exe_name.clone(),
                "libcalc.a".into(),
                "obj/calc/lib/src/lib.cpp.o".into()
            ]
        );
        assert_eq!(run_binary(&exe(&dir, "Debug", "calc")), "7\n");

        // A deleted object is rebuilt on its own.
        fs::remove_file(dir.join("target/Debug/obj/calc/bin/calc/src/main.cpp.o")).unwrap();
        let before = outputs(&dir);
        run(&build_args(&dir)).unwrap();
        assert_eq!(
            rebuilt(&before, &outputs(&dir)),
            ["obj/calc/bin/calc/src/main.cpp.o"]
        );

        // Different flags rebuild everything.
        let manifest = fs::read_to_string(dir.join("Yam.toml")).unwrap();
        assert!(manifest.contains("std = \"c++23\""), "{manifest}");
        write(
            &dir.join("Yam.toml"),
            &manifest.replace("std = \"c++23\"", "std = \"c++26\""),
        );
        let before = outputs(&dir);
        run(&build_args(&dir)).unwrap();
        let after = outputs(&dir);
        // Every object and interface is recompiled. The archive and executable may
        // then be left alone, if those came out identical.
        let compiled = before
            .keys()
            .filter(|p| p.starts_with("obj/") || p.starts_with("modules/"));
        for path in compiled {
            assert!(
                rebuilt(&before, &after).contains(path),
                "{path} wasn't rebuilt"
            );
        }
        assert_eq!(run_binary(&exe(&dir, "Debug", "calc")), "7\n");
    }

    /// Like ninja's `restat`: a recompile whose output comes out byte-identical
    /// doesn't make what depends on it stale. Release builds have no debug info, whose
    /// source checksums would otherwise make every edit change the object.
    #[test]
    fn identical_outputs_stop_the_rebuild() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("same");
        new_project(&dir, true, true);
        write(
            &dir.join("src/main.cpp"),
            "#include <cstdio>\n#include \"same.hpp\"\n\nint main() {\n    std::printf(\"%d\\n\", same::add(1, 2));\n}\n",
        );
        let mut release = build_args(&dir);
        release.release = true;
        run(&release).unwrap();
        let release_outputs = |dir: &Path| {
            let debug = dir.join("target/Release");
            ["libsame.a", "obj/same/lib/src/lib.cpp.o"]
                .map(|p| fs::metadata(debug.join(p)).unwrap().modified().unwrap())
        };
        let before = release_outputs(&dir);
        let text = fs::read_to_string(dir.join("src/lib.cpp")).unwrap();
        write(
            &dir.join("src/lib.cpp"),
            &format!("{text}// a trailing comment\n"),
        );
        run(&release).unwrap();
        let after = release_outputs(&dir);
        assert_ne!(before[1], after[1], "the object was recompiled");
        assert_eq!(
            before[0], after[0],
            "the identical object didn't re-archive"
        );
        assert_eq!(run_binary(&exe(&dir, "Release", "same")), "3\n");
    }

    #[test]
    fn a_header_edit_recompiles_only_its_includers() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("headers");
        write(
            &dir.join("Yam.toml"),
            "[project]\nname = \"headers\"\nstd = \"c++17\"\n",
        );
        write(
            &dir.join("src/value.h"),
            "inline int value() { return 1; }\n",
        );
        write(
            &dir.join("src/uses.cpp"),
            "#include \"value.h\"\nint uses() { return value(); }\n",
        );
        write(&dir.join("src/other.cpp"), "int other() { return 10; }\n");
        write(
            &dir.join("src/main.cpp"),
            "#include <cstdio>\nint uses();\nint other();\nint main() { std::printf(\"%d\\n\", uses() + other()); }\n",
        );
        write(
            &dir.join("Yam.toml"),
            "[project]\nname = \"headers\"\nstd = \"c++17\"\n\n[[bin]]\nname = \"headers\"\nsources = [\"src/*.cpp\"]\n",
        );
        run(&build_args(&dir)).unwrap();
        assert_eq!(run_binary(&exe(&dir, "Debug", "headers")), "11\n");

        write(
            &dir.join("src/value.h"),
            "inline int value() { return 2; }\n",
        );
        let before = outputs(&dir);
        run(&build_args(&dir)).unwrap();
        assert_eq!(
            rebuilt(&before, &outputs(&dir)),
            [
                format!("headers{}", std::env::consts::EXE_SUFFIX),
                "obj/headers/bin/headers/src/uses.cpp.o".into()
            ]
        );
        assert_eq!(run_binary(&exe(&dir, "Debug", "headers")), "12\n");
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

    /// A build that fails before anything is compiled.
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
        if !have_toolchain() {
            return;
        }
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
        if !have_toolchain() {
            return;
        }
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
    fn failed_scans_are_reported() {
        if !have_toolchain() {
            return;
        }
        let err = build_error(
            "[project]\nname = \"app\"\n",
            &[("src/main.cpp", "import <vector>;\nint main() {}\n")],
        );
        assert!(
            err.starts_with("could not scan the project's module dependencies"),
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
