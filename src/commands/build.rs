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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::cli::BuildArgs;
use crate::engine::{
    self, Action, CommandHash, CommandHasher, Deps, FlagsHash, GraphTarget, HeaderDigest,
    HeaderRecord, Key, ModuleName, Plan, STD_MODULE, ScanJob, SourceFile, State,
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
    let profile = if args.release {
        Profile::Release
    } else {
        Profile::Debug
    };
    let jobs = args.jobs.map_or_else(
        || std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        |jobs| jobs as usize,
    );
    let out = root.join("target").join(profile.dir_name());
    let mut caches = Caches::load(&out);

    // Every file the last build recorded is about to be checked. Their `stat`s run
    // on worker threads while this one reads the manifest, walks the sources and
    // checks the toolchain, so on a no-op build they come almost for free.
    let mut prefetch = caches.state.outputs.prefetch_plan(jobs);
    let cached_toolchain = caches.state.toolchain().cloned();
    let state = &mut caches.state;
    let prepared = std::thread::scope(|scope| {
        let workers = state.outputs.prefetch_spawn(scope, &mut prefetch);
        let prepared = (|| -> Result<_> {
            let manifest = Manifest::load(&root)?;
            let tree = SourceTree::scan_cached(&root, &mut state.dirs)?;
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
            let toolchain = Toolchain::discover_cached(cached_toolchain.as_ref())?;
            Ok((manifest, tree, targets, toolchain))
        })();
        for worker in workers {
            // A worker that panicked leaves the rest of its files unchecked, to be
            // checked as they come up.
            let _ = worker.join();
        }
        prepared
    });
    caches.state.outputs.prefetch_apply(prefetch);
    let (manifest, tree, targets, (toolchain, discovered)) = prepared?;
    let selected = select_files(&root, &targets, &tree)?;
    if let Some(discovered) = discovered {
        caches.state.set_toolchain(discovered);
    }

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
        project: &manifest.project.name,
        std: manifest.project.std,
        profile,
        toolchain: &toolchain,
        verbose: args.verbose,
        jobs,
        keep_going: args.keep_going,
        caches: RefCell::new(caches),
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

/// Scan and order the selected targets, then run the build graph: up to `-j` nodes
/// at a time, each once everything it needs is done, skipping every node whose
/// output is fresh.
///
/// After the first failure no new node starts, and the build fails once the running
/// ones finish, like Cargo. With `--keep-going`, every node that doesn't depend on a
/// failed one still runs, and every failure is reported.
fn build(builder: &Builder<'_>, selected: Vec<Selected<'_>>) -> Result<()> {
    let prepared = builder.prepare(selected)?;
    builder.caches.borrow_mut().flush();
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
    let std_module = nodes
        .iter()
        .any(|n| n.action == Action::Std)
        .then(|| builder.std_module());
    let flags: Vec<CompileFlags> = prepared
        .iter()
        .map(|p| builder.compile_flags(p, std_module.as_ref(), Builder::lib_for(p, lib.as_ref())))
        .collect();

    let mut progress = Progress::new(&nodes);

    let (send, receive) = mpsc::channel::<Finished>();
    let mut running: HashMap<usize, Finish> = HashMap::new();
    let mut failures: Vec<String> = Vec::new();
    let (mut ran, mut compiles) = (0, 0);

    loop {
        // After a failure, start nothing new unless keeping going.
        while running.len() < builder.jobs && (failures.is_empty() || builder.keep_going) {
            let Some(id) = progress.next() else {
                break;
            };
            let pcms = imports(&nodes, id, &prepared, std_module.as_ref());
            let context = NodeContext {
                prepared: &prepared,
                flags: &flags,
                lib: lib.as_ref(),
                std: std_module.as_ref(),
                pcms: &pcms,
            };
            if matches!(nodes[id].action, Action::Compile { .. }) {
                compiles += 1;
            }
            match builder.job(nodes[id].action, &context) {
                Ok(None) => progress.done(id),
                Ok(Some(job)) => {
                    running.insert(id, spawn(id, job, &send, builder.verbose));
                }
                Err(err) => failures.push(format!("{err:#}")),
            }
        }
        if running.is_empty() {
            break;
        }

        let Finished {
            node,
            output,
            error,
        } = receive
            .recv()
            .context("a build job stopped without reporting back")?;
        let finish = running
            .remove(&node)
            .context("a build job reported back twice")?;
        print_output(&output);
        let result = match error {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => builder.finish(finish, &prepared),
        };
        match result {
            Ok(was_compile) => {
                ran += usize::from(was_compile);
                builder.caches.borrow_mut().flush();
                progress.done(node);
            }
            Err(err) => {
                if failures.is_empty() && !builder.keep_going && !running.is_empty() {
                    style::warning("build failed, waiting for other jobs to finish...");
                }
                failures.push(format!("{err:#}"));
            }
        }
    }

    if builder.verbose {
        style::status("Compiled", format!("{ran} of {compiles} files"));
    }
    // Each distinct failure once, with a count when a target's files failed more
    // than once. All but the last are printed here; the last becomes the error.
    let mut summary = failure_summary(failures);
    let last = summary.pop();
    for failure in &summary {
        style::error(failure);
    }
    match last {
        Some(last) => bail!("{last}"),
        None => Ok(()),
    }
}

/// Failures in the order they happened, each message once: ``could not compile `app`
/// (bin): 2 files failed`` when two of its files did.
fn failure_summary(failures: Vec<String>) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for failure in failures {
        match counts.iter_mut().find(|(message, _)| *message == failure) {
            Some((_, count)) => *count += 1,
            None => counts.push((failure, 1)),
        }
    }
    counts
        .into_iter()
        .map(|(message, count)| match count {
            1 => message,
            n => format!("{message}: {n} files failed"),
        })
        .collect()
}

/// Which nodes are ready to run: every node they need is done.
struct Progress {
    /// How many of its needs each node still waits for.
    waiting: Vec<usize>,
    /// The nodes that need each node.
    dependents: Vec<Vec<usize>>,
    /// Ready nodes, lowest first: the graph's order puts module interfaces, which
    /// unblock the most, early.
    ready: BTreeSet<usize>,
}

impl Progress {
    fn new(nodes: &[engine::Node]) -> Self {
        let waiting: Vec<usize> = nodes.iter().map(|n| n.needs.len()).collect();
        let mut dependents = vec![Vec::new(); nodes.len()];
        for (id, node) in nodes.iter().enumerate() {
            for &need in &node.needs {
                dependents[need].push(id);
            }
        }
        let ready = (0..nodes.len()).filter(|&id| waiting[id] == 0).collect();
        Self {
            waiting,
            dependents,
            ready,
        }
    }

    fn next(&mut self) -> Option<usize> {
        self.ready.pop_first()
    }

    /// Node `id` is done; whatever was waiting only on it is now ready. A failed node
    /// is never marked done, so nothing downstream of it ever runs.
    fn done(&mut self, id: usize) {
        for &dependent in &self.dependents[id] {
            self.waiting[dependent] -= 1;
            if self.waiting[dependent] == 0 {
                self.ready.insert(dependent);
            }
        }
    }
}

/// Start `job` on a worker thread, which reports back on `send`. Returns what to
/// record once it succeeds.
fn spawn(id: usize, job: Job, send: &mpsc::Sender<Finished>, verbose: bool) -> Finish {
    if verbose {
        for command in &job.commands {
            style::status("Running", format!("`{}`", display(command)));
        }
    }
    let send = send.clone();
    let (commands, failure) = (job.commands, job.failure);
    std::thread::spawn(move || {
        let _ = send.send(run_commands(id, commands, &failure));
    });
    job.finish
}

/// The compiled interfaces node `id` imports directly. Those are the only `.pcm`
/// inputs its staleness needs: Clang records the signature of each module a `.pcm`
/// imports in the `.pcm` itself, so a change anywhere below a direct import changes
/// that import's bytes too, and an interface that comes out byte-identical means
/// nothing below it changed.
fn imports<'a>(
    nodes: &[engine::Node],
    id: usize,
    prepared: &'a [Prepared<'_>],
    std: Option<&'a StdModule>,
) -> Vec<Interned<'a>> {
    let mut pcms = Vec::with_capacity(nodes[id].needs.len());
    for &need in &nodes[id].needs {
        match nodes[need].action {
            Action::Std => pcms.extend(std.map(|std| std.pcm.as_ref())),
            Action::Compile { target, step } => {
                let file = prepared[target].plan.steps[step].file;
                pcms.extend(
                    prepared[target].paths[file]
                        .pcm
                        .as_ref()
                        .map(Interned::from),
                );
            }
            Action::Archive { .. } | Action::Link { .. } => {}
        }
    }
    pcms.sort_by(|a, b| a.path.cmp(b.path));
    pcms
}

/// A path with its id in the output record, interned once per build.
struct InternedPath {
    path: PathBuf,
    id: u32,
}

impl InternedPath {
    fn new(path: PathBuf, outputs: &mut engine::Outputs) -> Self {
        let id = outputs.intern(&path);
        Self { path, id }
    }

    fn as_ref(&self) -> Interned<'_> {
        Interned::from(self)
    }
}

/// A borrowed [`InternedPath`].
#[derive(Clone, Copy)]
struct Interned<'a> {
    path: &'a Path,
    id: u32,
}

impl<'a> From<&'a InternedPath> for Interned<'a> {
    fn from(interned: &'a InternedPath) -> Self {
        Self {
            path: &interned.path,
            id: interned.id,
        }
    }
}

/// What a node's job needs to know beyond the node itself.
struct NodeContext<'a, 't> {
    prepared: &'a [Prepared<'t>],
    /// By target, the compile flags its files share.
    flags: &'a [CompileFlags],
    lib: Option<&'a Lib>,
    std: Option<&'a StdModule>,
    /// The compiled interfaces this node imports directly.
    pcms: &'a [Interned<'a>],
}

/// The start of every compile command in a target: the compiler and the flags all
/// its files share, with their hash so far. Worked out once per target, so a no-op
/// build doesn't rebuild and rehash the same arguments for every file.
struct CompileFlags {
    args: Vec<OsString>,
    hasher: CommandHasher,
}

/// A node's commands, run in order on a worker thread, and how the build records
/// their result.
struct Job {
    commands: Vec<Command>,
    /// The error if a command fails, e.g. ``could not compile `app` (bin)``.
    failure: String,
    finish: Finish,
}

/// What the build records once a job's commands succeed.
enum Finish {
    Std {
        stamp_path: PathBuf,
        stamp: String,
    },
    Compile {
        target: usize,
        file: usize,
        source: PathBuf,
        object: PathBuf,
        pcm: Option<PathBuf>,
        depfile: PathBuf,
        pcms: Vec<PathBuf>,
        command: CommandHash,
    },
    Output {
        output: PathBuf,
        command: CommandHash,
        inputs: Vec<PathBuf>,
    },
}

/// A job's result, sent back from its worker thread.
struct Finished {
    node: usize,
    /// Each command's captured stdout and stderr, in order.
    output: Vec<(Vec<u8>, Vec<u8>)>,
    /// Why it failed, if it did.
    error: Option<String>,
}

/// Run `commands` in order, capturing their output, stopping at the first failure.
fn run_commands(node: usize, commands: Vec<Command>, failure: &str) -> Finished {
    let mut output = Vec::with_capacity(commands.len());
    for mut command in commands {
        match command.output() {
            Ok(result) => {
                let ok = result.status.success();
                output.push((result.stdout, result.stderr));
                if !ok {
                    return Finished {
                        node,
                        output,
                        error: Some(failure.to_string()),
                    };
                }
            }
            Err(err) => {
                return Finished {
                    node,
                    output,
                    error: Some(format!(
                        "{failure}: couldn't run `{}`: {err}",
                        display(&command)
                    )),
                };
            }
        }
    }
    Finished {
        node,
        output,
        error: None,
    }
}

/// Print a finished job's output as one block. Colors in it are kept on a terminal
/// and stripped otherwise, following `--color`.
fn print_output(output: &[(Vec<u8>, Vec<u8>)]) {
    use std::io::Write as _;
    for (stdout, stderr) in output {
        if !stdout.is_empty() {
            let _ = anstream::stdout().write_all(stdout);
        }
        if !stderr.is_empty() {
            let _ = anstream::stderr().write_all(stderr);
        }
    }
}

/// What one build hands the next, in the build state file, plus what this build
/// learns along the way. Loaded before the source walk; saved at the end even if the
/// build fails, and journaled as jobs finish, so a killed build keeps its progress.
struct Caches {
    state: State,
    /// What this build knows of each recorded header, by header id: resolved the
    /// first time any file needs it.
    header_info: Vec<Option<HeaderInfo>>,
    /// Whether writing the journal already failed, so that's reported once.
    journal_failed: bool,
}

impl Caches {
    fn load(out: &Path) -> Self {
        let (state, errors) = State::load(&out.join(".yam"));
        for reason in errors {
            style::note(format!("{reason}; building everything again"));
        }
        Self {
            state,
            header_info: Vec::new(),
            journal_failed: false,
        }
    }

    /// Append what finished since the last call to the journal.
    fn flush(&mut self) {
        if let Err(err) = self.state.flush()
            && !self.journal_failed
        {
            self.journal_failed = true;
            style::warning(format!(
                "couldn't write the build journal: {err}; an interrupted build will redo its work"
            ));
        }
    }

    /// The scan key for a file, using the headers `object` was last compiled with.
    fn key(&mut self, root: &Path, file: &FileKey, object: u32) -> Key {
        let state = &mut self.state;
        let digests: Vec<HeaderDigest> = state
            .headers
            .get(object)
            .iter()
            .map(|&id| {
                resolve_header(
                    &mut self.header_info,
                    &state.headers,
                    &mut state.outputs,
                    root,
                    id,
                )
                .digest
            })
            .collect();
        Key::new(&file.flags, &file.contents, &digests)
    }

    fn save(&mut self) {
        if let Err(err) = self.state.save() {
            style::warning(format!("couldn't save the build state: {err}"));
        }
    }
}

/// One recorded header as this build sees it.
#[derive(Clone, Copy)]
struct HeaderInfo {
    /// Its id in the output record.
    path: u32,
    /// Its path and contents, for scan keys.
    digest: HeaderDigest,
}

/// Header `id`'s info, worked out once per build: its path is joined, looked up and
/// hashed here, rather than for every file that includes it.
fn resolve_header(
    memo: &mut Vec<Option<HeaderInfo>>,
    headers: &HeaderRecord,
    outputs: &mut engine::Outputs,
    root: &Path,
    id: u32,
) -> HeaderInfo {
    let slot = id as usize;
    if let Some(Some(info)) = memo.get(slot) {
        return *info;
    }
    let name = headers.name(id);
    let path = outputs.intern(&root.join(name));
    let digest = HeaderDigest::new(name, outputs.contents_of(path).as_ref());
    let info = HeaderInfo { path, digest };
    if memo.len() <= slot {
        memo.resize(slot + 1, None);
    }
    memo[slot] = Some(info);
    info
}

/// A scanned file: its module dependencies, scan key parts, source and object.
struct Scanned {
    deps: Deps,
    key: FileKey,
    source: InternedPath,
    object: InternedPath,
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
    /// Each file's paths, in the same order as `files`.
    paths: Vec<FilePaths>,
    plan: Plan,
}

/// Where a file is and what compiling it writes, joined and interned once per build.
struct FilePaths {
    source: InternedPath,
    object: InternedPath,
    depfile: PathBuf,
    /// Its compiled interface, if it provides a module.
    pcm: Option<InternedPath>,
}

impl Prepared<'_> {
    /// The modules and partitions the target provides.
    fn modules(&self) -> HashSet<ModuleName> {
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
    pcm: InternedPath,
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
    /// How many jobs run at once.
    jobs: usize,
    /// Whether to keep going after a failure.
    keep_going: bool,
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

    /// Where `std.pcm` and `std.o` go: `std/<std>/`.
    fn std_module(&self) -> StdModule {
        let dir = self.out.join("std").join(self.std.as_str());
        StdModule {
            pcm: InternedPath::new(
                dir.join("std.pcm"),
                &mut self.caches.borrow_mut().state.outputs,
            ),
            object: dir.join("std.o"),
        }
    }

    /// Build `std.pcm` and `std.o`, unless the last build left them there from the
    /// same compiler, commands and `std.cppm`.
    fn std_job(&self, module: &StdModule) -> Result<Option<Job>> {
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
            .arg(&module.pcm.path);
        // Compiling a `.pcm` to an object reads no headers, so no `-stdlib`.
        let mut compile = Command::new(&self.toolchain.cxx.path);
        compile
            .arg(format!("-std={}", self.std.as_str()))
            .args(self.profile.flags())
            .arg("-c")
            .arg(&module.pcm.path)
            .arg("-o")
            .arg(&module.object);

        // Until the build state file (#26), a stamp next to the outputs records what
        // they were built from.
        let dir = module.pcm.path.parent().unwrap_or(&self.out).to_path_buf();
        let stamp_path = dir.join("std.stamp");
        let stamp = [
            identity(&self.toolchain.cxx.path),
            self.toolchain.cxx.version.to_string(),
            identity(&std.source),
            display(&precompile),
            display(&compile),
        ]
        .join("\n");
        let fresh = module.pcm.path.is_file()
            && module.object.is_file()
            && fs::read_to_string(&stamp_path).is_ok_and(|old| old == stamp);
        if fresh {
            return Ok(None);
        }

        create_dir(&dir)?;
        // Remove the stamp first so an interrupted build can't look fresh.
        let _ = fs::remove_file(&stamp_path);
        Ok(Some(Job {
            commands: vec![precompile, compile],
            failure: "failed to build the `std` module".into(),
            finish: Finish::Std { stamp_path, stamp },
        }))
    }

    /// The job for a node, or `None` if its outputs are fresh.
    fn job(&self, action: Action, context: &NodeContext<'_, '_>) -> Result<Option<Job>> {
        match action {
            Action::Std => match context.std {
                Some(module) => self.std_job(module),
                None => Ok(None),
            },
            Action::Compile { target, step } => self.compile_job(
                target,
                &context.prepared[target],
                step,
                &context.flags[target],
                context.pcms,
            ),
            Action::Archive { target } => self.archive_job(&context.prepared[target]),
            Action::Link { target } => {
                let prepared = &context.prepared[target];
                let lib = context.lib.filter(|_| prepared.target.links_lib);
                self.link_job(prepared, context.std, lib)
            }
        }
    }

    /// Record a job that succeeded. Returns whether it was a compile.
    fn finish(&self, finish: Finish, prepared: &[Prepared<'_>]) -> Result<bool> {
        match finish {
            Finish::Std { stamp_path, stamp } => {
                fs::write(&stamp_path, stamp)
                    .with_context(|| format!("failed to write `{}`", stamp_path.display()))?;
                Ok(false)
            }
            Finish::Compile {
                target,
                file,
                source,
                object,
                pcm,
                depfile,
                pcms,
                command,
            } => {
                let headers = self.record_headers(&prepared[target], file, &object, &depfile);
                let inputs = self.compile_inputs(&source, &headers, &pcms);
                let mut caches = self.caches.borrow_mut();
                caches.state.outputs.record(&object, command, &inputs);
                caches.state.outputs.invalidate(&object);
                if let Some(pcm) = &pcm {
                    caches.state.outputs.invalidate(pcm);
                }
                Ok(true)
            }
            Finish::Output {
                output,
                command,
                inputs,
            } => {
                let mut caches = self.caches.borrow_mut();
                caches.state.outputs.record(&output, command, &inputs);
                caches.state.outputs.invalidate(&output);
                Ok(false)
            }
        }
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
            let (_, module_dir) = self.target_dirs(target);
            let mut caches = self.caches.borrow_mut();
            let outputs = &mut caches.state.outputs;
            let mut keys = Vec::with_capacity(files.len());
            let mut paths = Vec::with_capacity(files.len());
            let files: Vec<SourceFile> = files
                .into_iter()
                .zip(scanned.by_ref())
                .map(|(path, scanned)| {
                    let pcm = scanned.deps.provides.as_ref().map(|provided| {
                        InternedPath::new(module_dir.join(pcm_name(&provided.name)), outputs)
                    });
                    paths.push(FilePaths {
                        depfile: depfile_path(&scanned.object.path),
                        object: scanned.object,
                        source: scanned.source,
                        pcm,
                    });
                    keys.push(scanned.key);
                    SourceFile {
                        path,
                        deps: scanned.deps,
                    }
                })
                .collect();
            drop(caches);
            let available = if target.kind == TargetKind::Bin && target.links_lib {
                &lib_modules
            } else {
                &none
            };
            let plan = engine::plan(&describe(target), &files, available)?;
            if plan.imports_std && !self.std.supports_import_std() {
                let file = files
                    .iter()
                    .find(|f| f.deps.requires.iter().any(|m| &**m == STD_MODULE))
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
                paths,
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
    fn scan(&self, selected: &[Selected<'_>], lib_target: Option<&Target>) -> Result<Vec<Scanned>> {
        let toolchain = self.toolchain;
        let tools = format!(
            "{} {} {} {}",
            toolchain.scan_deps.path.display(),
            toolchain.scan_deps.version,
            toolchain.cxx.path.display(),
            toolchain.cxx.version
        );
        let mut caches = self.caches.borrow_mut();

        let mut results: Vec<(Option<Deps>, FileKey, InternedPath, InternedPath)> = Vec::new();
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
                let source = InternedPath::new(self.root.join(file), &mut caches.state.outputs);
                let contents = caches
                    .state
                    .outputs
                    .contents_of(source.id)
                    .with_context(|| format!("failed to read `{file}`"))?;
                let file_key = FileKey {
                    flags: flags.clone(),
                    contents,
                };
                let output =
                    InternedPath::new(object_file(&obj_dir, file), &mut caches.state.outputs);
                let key = caches.key(self.root, &file_key, output.id);
                if let Some(deps) = caches.state.scan.get(key) {
                    results.push((Some(deps), file_key, source, output));
                    continue;
                }
                create_parent(&output.path)?;
                let arguments = std::iter::once(command.get_program())
                    .chain(command.get_args())
                    .map(ToOwned::to_owned)
                    .chain([
                        "-c".into(),
                        source.path.clone().into_os_string(),
                        "-o".into(),
                        output.path.clone().into_os_string(),
                    ])
                    .collect();
                misses.push((results.len(), key));
                jobs.push(ScanJob {
                    source: source.path.clone(),
                    output: output.path.clone(),
                    arguments,
                });
                results.push((None, file_key, source, output));
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
                caches.state.scan.insert(key, deps.clone());
                results[index].0 = Some(deps);
            }
        }
        Ok(results
            .into_iter()
            .filter_map(|(deps, key, source, object)| {
                Some(Scanned {
                    deps: deps?,
                    key,
                    source,
                    object,
                })
            })
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

    /// The library as `prepared`'s compiles and link use it: only a bin that links
    /// it does.
    fn lib_for<'l>(prepared: &Prepared<'_>, lib: Option<&'l Lib>) -> Option<&'l Lib> {
        lib.filter(|_| prepared.target.kind == TargetKind::Bin && prepared.target.links_lib)
    }

    /// The compiler and flags every compile in `prepared` starts with, and their
    /// hash so far.
    fn compile_flags(
        &self,
        prepared: &Prepared<'_>,
        std: Option<&StdModule>,
        lib: Option<&Lib>,
    ) -> CompileFlags {
        let target = prepared.target;
        let mut args: Vec<OsString> = vec![
            self.toolchain.cxx.path.clone().into_os_string(),
            format!("-std={}", self.std.as_str()).into(),
            "-stdlib=libc++".into(),
        ];
        args.extend(self.profile.flags().iter().map(OsString::from));
        if let Some(std) = std {
            args.push(format!("-fmodule-file=std={}", std.pcm.path.display()).into());
        }
        let (_, module_dir) = self.target_dirs(target);
        let own_modules = prepared.plan.steps.iter().any(|s| s.provides.is_some());
        let module_dirs = own_modules
            .then_some(&module_dir)
            .into_iter()
            .chain(lib.and_then(|lib| lib.module_dir.as_ref()));
        for dir in module_dirs {
            args.push(format!("-fprebuilt-module-path={}", dir.display()).into());
        }
        let include_dirs = target
            .include_dirs
            .iter()
            .map(|dir| self.root.join(dir))
            .chain(
                lib.into_iter()
                    .flat_map(|lib| lib.include_dirs.iter().cloned()),
            );
        for dir in include_dirs {
            args.push(format!("-I{}", dir.display()).into());
        }
        let mut hasher = CommandHasher::new(&self.tools());
        for arg in &args {
            hasher.arg(arg);
        }
        // What every file adds to these, written with placeholders for the file and
        // module: see `compile_hash`.
        let (obj_dir, _) = self.target_dirs(target);
        let source = self.root.join(TEMPLATE_FILE);
        let object = object_file(&obj_dir, TEMPLATE_FILE);
        let depfile = depfile_path(&object);
        let pcm = module_dir.join(pcm_name(TEMPLATE_MODULE));
        hasher.encoded(b"\0\x01per-file");
        for (pcm, needs_language) in [(Some(&pcm), true), (Some(&pcm), false), (None, false)] {
            hasher.encoded(b"\0\x01variant");
            for arg in compile_suffix(
                pcm.map(PathBuf::as_path),
                needs_language,
                &depfile,
                &source,
                &object,
            ) {
                hasher.arg(arg);
            }
        }
        CompileFlags { args, hasher }
    }

    /// The object a target's plan step compiles to.
    fn object<'p>(prepared: &'p Prepared<'_>, step: usize) -> &'p Path {
        &prepared.paths[prepared.plan.steps[step].file].object.path
    }

    /// The job compiling one file, unless its object (and interface) are fresh.
    /// `pcms` are the compiled interfaces it reads.
    ///
    /// A fresh file costs a hash of its last few arguments and a `stat` per input:
    /// the command itself is only put together when it has to run.
    fn compile_job(
        &self,
        target_index: usize,
        prepared: &Prepared<'_>,
        step: usize,
        flags: &CompileFlags,
        pcms: &[Interned<'_>],
    ) -> Result<Option<Job>> {
        let target = prepared.target;
        let file = prepared.plan.steps[step].file;
        let paths = &prepared.paths[file];
        // `.cppm` is a module interface by its extension; anything else that
        // declares a module needs saying so.
        let needs_language =
            paths.pcm.is_some() && !engine::has_extension(&prepared.files[file].path, "cppm");
        let module = prepared.files[file].deps.provides.as_ref();
        let command = compile_hash(
            flags,
            &prepared.files[file].path,
            module.map(|m| &*m.name),
            needs_language,
        );

        let fresh = {
            let mut caches = self.caches.borrow_mut();
            let Caches {
                state, header_info, ..
            } = &mut *caches;
            let headers = state.headers.get(paths.object.id);
            let mut inputs = Vec::with_capacity(1 + headers.len() + pcms.len());
            inputs.push(paths.source.id);
            for &id in headers {
                let info = resolve_header(
                    header_info,
                    &state.headers,
                    &mut state.outputs,
                    self.root,
                    id,
                );
                inputs.push(info.path);
            }
            inputs.extend(pcms.iter().map(|pcm| pcm.id));
            state
                .outputs
                .is_fresh_ids(paths.object.id, command, &inputs)
                && paths
                    .pcm
                    .as_ref()
                    .is_none_or(|pcm| state.outputs.exists(pcm.id))
        };
        if fresh {
            return Ok(None);
        }

        let mut compile = Command::new(&flags.args[0]);
        compile.args(&flags.args[1..]).args(compile_suffix(
            paths.pcm.as_ref().map(|pcm| pcm.path.as_path()),
            needs_language,
            &paths.depfile,
            &paths.source.path,
            &paths.object.path,
        ));

        create_parent(&paths.object.path)?;
        if let Some(pcm) = &paths.pcm {
            create_parent(&pcm.path)?;
        }
        Ok(Some(Job {
            commands: vec![compile],
            failure: could_not("compile", target),
            finish: Finish::Compile {
                target: target_index,
                file,
                source: paths.source.path.clone(),
                object: paths.object.path.clone(),
                pcm: paths.pcm.as_ref().map(|pcm| pcm.path.clone()),
                depfile: paths.depfile.clone(),
                pcms: pcms.iter().map(|pcm| pcm.path.to_path_buf()).collect(),
                command,
            },
        }))
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
        let id = prepared.paths[file].object.id;
        let mut caches = self.caches.borrow_mut();
        caches
            .state
            .headers
            .record(id, &object.to_string_lossy(), headers.clone());
        let key = caches.key(self.root, &prepared.keys[file], id);
        caches
            .state
            .scan
            .insert(key, prepared.files[file].deps.clone());
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

    /// A target's objects' ids in the output record, in plan order.
    fn object_ids(prepared: &Prepared<'_>) -> Vec<u32> {
        prepared
            .plan
            .steps
            .iter()
            .map(|step| prepared.paths[step.file].object.id)
            .collect()
    }

    /// A target's objects, in plan order.
    fn objects(prepared: &Prepared<'_>) -> Vec<PathBuf> {
        (0..prepared.plan.steps.len())
            .map(|step| Self::object(prepared, step).to_path_buf())
            .collect()
    }

    /// The job archiving the library's objects, unless the archive is fresh.
    fn archive_job(&self, prepared: &Prepared<'_>) -> Result<Option<Job>> {
        let target = prepared.target;
        let archive = self.out.join(format!("lib{}.a", target.name));
        let before = [OsString::from("rcs"), archive.clone().into_os_string()];
        let command = CommandHash::new(
            &self.toolchain.ar.display().to_string(),
            before
                .iter()
                .map(OsString::as_os_str)
                .chain([OsStr::new(OBJECTS)]),
        );
        let fresh = {
            let outputs = &mut self.caches.borrow_mut().state.outputs;
            let archive = outputs.intern(&archive);
            outputs.is_fresh_ids(archive, command, &Self::object_ids(prepared))
        };
        if fresh {
            return Ok(None);
        }

        let objects = Self::objects(prepared);
        let _ = fs::remove_file(&archive);
        let mut ar = Command::new(&self.toolchain.ar);
        ar.args(&before);
        self.add_files(&mut ar, &objects, &format!("lib{}.a", target.name))?;
        Ok(Some(Job {
            commands: vec![ar],
            failure: could_not("archive", target),
            finish: Finish::Output {
                output: archive,
                command,
                inputs: objects,
            },
        }))
    }

    /// The job linking a bin, unless its executable is fresh.
    fn link_job(
        &self,
        prepared: &Prepared<'_>,
        std: Option<&StdModule>,
        lib: Option<&Lib>,
    ) -> Result<Option<Job>> {
        let target = prepared.target;
        let executable = self
            .out
            .join(format!("{}{}", target.name, std::env::consts::EXE_SUFFIX));

        // Everything after the objects; library files are inputs too, after the
        // objects.
        let mut after: Vec<OsString> = Vec::new();
        let mut libraries = Vec::new();
        if let Some(lib) = lib {
            after.push(lib.archive.clone().into_os_string());
            libraries.push(lib.archive.clone());
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
                    libraries.push(path);
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
                .chain([before.as_os_str(), OsStr::new(OBJECTS)])
                .chain(after.iter().map(OsString::as_os_str)),
        );
        let fresh = {
            let outputs = &mut self.caches.borrow_mut().state.outputs;
            let mut ids = Self::object_ids(prepared);
            ids.extend(std.map(|std| outputs.intern(&std.object)));
            ids.extend(libraries.iter().map(|path| outputs.intern(path)));
            let executable = outputs.intern(&executable);
            outputs.is_fresh_ids(executable, command, &ids)
        };
        if fresh {
            return Ok(None);
        }

        let mut objects = Self::objects(prepared);
        objects.extend(std.map(|std| std.object.clone()));
        let mut inputs = objects.clone();
        inputs.extend(libraries);
        let mut link = Command::new(&self.toolchain.cxx.path);
        link.arg(&before);
        self.add_files(&mut link, &objects, &target.name)?;
        link.args(&after);
        Ok(Some(Job {
            commands: vec![link],
            failure: could_not("link", target),
            finish: Finish::Output {
                output: executable,
                command,
                inputs,
            },
        }))
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
}

/// Stands for a target's objects in archive and link command hashes. The objects
/// themselves aren't hashed: they're the step's first inputs, which the output
/// record compares one by one, in order, so a different list is already stale.
const OBJECTS: &str = "\u{1}objects";
/// Stand-ins for the file and module in the per-file part of a target's compile
/// command, hashed once per target; see [`compile_hash`].
const TEMPLATE_FILE: &str = "\u{1}file";
const TEMPLATE_MODULE: &str = "\u{1}module";

/// The hash of the compile [`Builder::compile_job`] runs for `file` (relative to
/// the root), which provides `module`, without putting the command together.
///
/// A compile command is the target's shared flags, then arguments that depend only
/// on the file's path, its module and whether it needs `-x c++-module`. So
/// `flags`' hash covers the shared flags plus those per-file arguments written
/// with placeholders (see [`Builder::compile_flags`]), and each file adds just
/// its path and module. A change to how the per-file arguments are built changes
/// the placeholder version, and so every hash, and a different path, module or
/// language flag is a different hash; checking a fresh file hashes a few short
/// strings rather than every path in its command.
fn compile_hash(
    flags: &CompileFlags,
    file: &str,
    module: Option<&str>,
    needs_language: bool,
) -> CommandHash {
    let mut hasher = flags.hasher.clone();
    hasher.encoded(match (module, needs_language) {
        (Some(_), true) => b"\0\x01module, -x",
        (Some(_), false) => b"\0\x01module",
        (None, _) => b"\0\x01file",
    });
    hasher.arg(file);
    if let Some(module) = module {
        hasher.arg(module);
    }
    hasher.finish()
}

/// The arguments after a target's shared compile flags, for one file. Colors are
/// asked for explicitly because the output is captured; printing it strips them
/// again when they aren't wanted.
fn compile_suffix<'a>(
    pcm: Option<&'a Path>,
    needs_language: bool,
    depfile: &'a Path,
    source: &'a Path,
    object: &'a Path,
) -> Vec<OsString> {
    let mut args = Vec::with_capacity(12);
    if let Some(pcm) = pcm {
        let mut output = OsString::from("-fmodule-output=");
        output.push(pcm);
        args.push(output);
        if needs_language {
            args.extend(["-x".into(), "c++-module".into()]);
        }
    }
    args.extend([
        "-fdiagnostics-color=always".into(),
        "-MMD".into(),
        "-MF".into(),
        depfile.into(),
        "-c".into(),
        source.into(),
        "-o".into(),
        object.into(),
    ]);
    args
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
            jobs: None,
            keep_going: false,
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

    /// Age the project's directories by an hour and build once, so the directory
    /// cache trusts their listings: it doesn't trust a directory changed within the
    /// last two seconds. Directories can't be given an mtime this way on Windows,
    /// where these tests have no toolchain anyway.
    fn settle(dir: &Path) {
        #[cfg(unix)]
        {
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            for entry in walkdir::WalkDir::new(dir)
                .into_iter()
                .filter_entry(|e| e.file_name() != "target")
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_dir())
            {
                fs::File::open(entry.path())
                    .and_then(|file| file.set_modified(old))
                    .unwrap();
            }
        }
        run(&build_args(dir)).unwrap();
        let _ = scanned(dir);
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
        let cache = dir.join("target/Debug/.yam/state");

        run(&build_args(&dir)).unwrap();
        assert_eq!(scanned(&dir), Some(2));
        // The first build created `target/`, so the next one records the root's new
        // listing once; after that a no-op build changes nothing.
        settle(&dir);
        let saved = fs::metadata(&cache).unwrap().modified().unwrap();

        // A no-op build scans nothing and leaves the state file alone.
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
        assert!(dir.join("target/Release/.yam/state").is_file());

        // A corrupt cache is discarded, and everything is scanned again.
        fs::write(&cache, "not a state file").unwrap();
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
        // The state file stores strings as plain UTF-8, so the recorded headers can be
        // looked for in its bytes.
        let state = fs::read(dir.join("target/Debug/.yam/state")).unwrap();
        let has = |needle: &str| state.windows(needle.len()).any(|w| w == needle.as_bytes());
        assert!(has("src/config.h"), "the header isn't recorded");
        // The cached toolchain names libc++'s directory, but not a header in it.
        assert!(!has("v1/cstdio"), "toolchain headers aren't recorded");

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

    /// Only direct `.pcm` imports are staleness inputs, so a change two imports down
    /// must still reach `main` through the interfaces in between.
    #[test]
    fn a_change_deep_in_the_import_chain_reaches_every_importer() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("chain");
        let base = |value: i32| {
            format!("export module chain.base;\n\nexport inline int base() {{ return {value}; }}\n")
        };
        write(&dir.join("src/base.cppm"), &base(1));
        write(
            &dir.join("src/mid.cppm"),
            "export module chain.mid;\nimport chain.base;\n\nexport inline int mid() { return base() * 10; }\n",
        );
        write(
            &dir.join("src/lib.cppm"),
            "export module chain;\nexport import chain.mid;\n",
        );
        write(
            &dir.join("src/main.cpp"),
            "import std;\nimport chain;\n\nint main() { std::println(\"{}\", mid()); }\n",
        );
        write(&dir.join("Yam.toml"), "[project]\nname = \"chain\"\n");
        run(&build_args(&dir)).unwrap();
        assert_eq!(run_binary(&exe(&dir, "Debug", "chain")), "10\n");

        // `main` imports only `chain`, which reaches `chain.base` through `chain.mid`.
        write(&dir.join("src/base.cppm"), &base(2));
        let before = outputs(&dir);
        run(&build_args(&dir)).unwrap();
        let rebuilt = rebuilt(&before, &outputs(&dir));
        assert!(
            rebuilt.contains(&"obj/chain/bin/chain/src/main.cpp.o".to_string()),
            "{rebuilt:?}"
        );
        assert_eq!(run_binary(&exe(&dir, "Debug", "chain")), "20\n");
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

    /// A bin whose sources are, in compile order: a broken file, two good ones, and
    /// another broken one.
    fn two_broken_files(dir: &Path) {
        write(
            &dir.join("Yam.toml"),
            "[project]\nname = \"app\"\nstd = \"c++17\"\n\n[[bin]]\nname = \"app\"\nsources = [\"src/*.cpp\"]\n",
        );
        write(&dir.join("src/a_bad.cpp"), "int a() { return nope; }\n");
        write(&dir.join("src/b.cpp"), "int b() { return 2; }\n");
        write(&dir.join("src/main.cpp"), "int main() {}\n");
        write(&dir.join("src/z_bad.cpp"), "int z() { return nope; }\n");
    }

    fn object(dir: &Path, file: &str) -> PathBuf {
        dir.join("target/Debug/obj/app/bin/app/src")
            .join(format!("{file}.o"))
    }

    #[test]
    fn the_first_failure_stops_new_jobs() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("app");
        two_broken_files(&dir);
        let mut args = build_args(&dir);
        args.jobs = Some(1);
        let err = run(&args).unwrap_err().to_string();
        assert_eq!(err, "could not compile `app` (bin)");
        // One job at a time, in order: nothing after `a_bad.cpp` started.
        assert!(!object(&dir, "b.cpp").exists());
        assert!(!object(&dir, "main.cpp").exists());
    }

    #[test]
    fn keep_going_builds_everything_it_can() {
        if !have_toolchain() {
            return;
        }
        let temp = tempdir().unwrap();
        let dir = temp.path().join("app");
        two_broken_files(&dir);
        let mut args = build_args(&dir);
        args.jobs = Some(1);
        args.keep_going = true;
        let err = run(&args).unwrap_err().to_string();
        assert_eq!(err, "could not compile `app` (bin): 2 files failed");
        assert!(object(&dir, "b.cpp").exists());
        assert!(object(&dir, "main.cpp").exists());
        // The link needs the broken objects, so it never ran.
        assert!(!exe(&dir, "Debug", "app").exists());

        // Fixing both files rebuilds only them, then links.
        let good = fs::metadata(object(&dir, "b.cpp"))
            .unwrap()
            .modified()
            .unwrap();
        write(&dir.join("src/a_bad.cpp"), "int a() { return 1; }\n");
        write(&dir.join("src/z_bad.cpp"), "int z() { return 26; }\n");
        run(&args).unwrap();
        assert!(exe(&dir, "Debug", "app").exists());
        assert_eq!(
            fs::metadata(object(&dir, "b.cpp"))
                .unwrap()
                .modified()
                .unwrap(),
            good
        );
    }

    #[test]
    fn parallel_and_serial_builds_agree() {
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
        for jobs in [1, 8] {
            let _ = fs::remove_dir_all(dir.join("target"));
            let mut args = build_args(&dir);
            args.jobs = Some(jobs);
            run(&args).unwrap();
            assert_eq!(run_binary(&exe(&dir, "Debug", "calc")), "42\n", "-j {jobs}");
        }
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
    fn repeated_failures_are_counted_once() {
        let summary = failure_summary(vec![
            "could not compile `app` (bin)".into(),
            "could not compile `core` (lib)".into(),
            "could not compile `app` (bin)".into(),
        ]);
        assert_eq!(
            summary,
            [
                "could not compile `app` (bin): 2 files failed",
                "could not compile `core` (lib)"
            ]
        );
    }

    #[test]
    fn each_part_of_a_compile_changes_its_hash() {
        let flags = |tools: &str| CompileFlags {
            args: Vec::new(),
            hasher: CommandHasher::new(tools),
        };
        let base = flags("clang++ 23");
        let hashes = [
            compile_hash(&base, "src/a.cpp", None, false),
            compile_hash(&base, "src/b.cpp", None, false),
            compile_hash(&base, "src/a.cpp", Some("a"), false),
            compile_hash(&base, "src/a.cpp", Some("a"), true),
            compile_hash(&base, "src/a.cpp", Some("b"), false),
            compile_hash(&flags("clang++ 22"), "src/a.cpp", None, false),
        ];
        for (i, a) in hashes.iter().enumerate() {
            for b in &hashes[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(hashes[0], compile_hash(&base, "src/a.cpp", None, false));
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
