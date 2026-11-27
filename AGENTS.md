### Project Context

**YAM** (Yet Another Make) is a Cargo-style build tool for modern C++ (C++20 modules
first, legacy headers supported). It is written in Rust and published as the
`yet-another-make` crate, which installs the `yam` binary. The headline goal is to
**out-perform CMake + ninja**, measured by the benchmark gates in `docs/build.md`.

### Core Commands

* **Build**: cargo build
* **Run**: cargo run -- <args>   (e.g. `cargo run -- init /tmp/demo`)
* **Test**: cargo test --workspace
* **Format Check**: cargo fmt --all --check
* **Lint Check**: cargo clippy --workspace --all-targets -- -D warnings
* **Static Linux release build**: cargo build --release --target x86_64-unknown-linux-musl
* **Package contents check**: cargo package --list

CI (`.github/workflows/ci.yml`) runs fmt and package checks, plus clippy and tests on
Linux, macOS and Windows with Clang 22 installed. A change isn't done until all of
those pass.

### Shell Environment

* Always run shell commands via zsh in interactive mode (`zsh -ic "<command>"`), not a
  plain or non-interactive shell. This ensures `~/.zshrc` is sourced, so `PATH` and
  toolchain entries (cargo, clang, gh) resolve the same way they do in my terminal.

### Tool Enforcement Contracts (Strict)

* **Formatting (`rustfmt.toml`)**: all code must pass `cargo fmt --all --check`. Don't
  hand-format against it.
* **Linting (`[workspace.lints]` in `Cargo.toml`)**: clippy `pedantic` is on and CI
  runs with `-D warnings`. Fix findings rather than adding `#[allow]`. If an allow is
  genuinely right, scope it to the item and say why in a comment. `unsafe_code` is
  denied.
* **Package contents**: `Cargo.toml`'s `include` list is the allowlist for what ships
  to crates.io. Keep local tool config (`.cargo/`, `.claude/`) out of it.

### Architectural Conventions

* **Single published crate, Rust-style module hierarchy.** A crates.io package can't
  depend on unpublished path crates, so the code is one crate (`src/lib.rs` plus a
  thin `src/main.rs`). Components are modules in subdirectories (`cli/`, `manifest/`,
  `engine/`, `toolchain/`) rather than separate crates. `bench/` is a
  `publish = false` workspace member that nothing depends on.
* **Library vs binary.** `main.rs` only selects the global allocator (mimalloc, on
  every target) and calls `yet_another_make::run()`. Everything else lives in the
  library, so integration tests (`tests/`, using `assert_cmd`) can reach it.
* **Errors.** Use `anyhow` with `.context(...)` at the command layer. Use typed errors
  (`thiserror`) in reusable modules such as `manifest/` and `engine/`. User-facing
  messages go through `style` (`error:`, `warning:`, `help:`, `note:`, and right-aligned
  Cargo-style status verbs).
* **Portability precedence.** Prefer Rust `std`, then a well-maintained crate, and raw
  OS calls (`libc`, Win32) only when neither works. Never shell out where `std` or a
  crate does the job, and spawn compilers directly with `std::process`, never through
  a shell.
* **Performance is a requirement, not an afterthought.** The no-op and incremental
  paths must not spawn processes or allocate per file without reason. Follow the
  engine design in `docs/build.md`, and update that doc in the same PR when a
  Proposed section changes. Every performance change follows **Performance
  Discipline** below.
* **Never link LLVM into `yam`.** Module dependencies come from batched
  `clang-scan-deps -format=p1689`, plus the persistent scan cache. `yam-iface` is a
  separate tool built by `yam-toolchain`.
* **`std.pcm`/`std.compat.pcm`** are built into the build directory, one per distinct
  project standard that uses `import std`, and staleness-tracked like any object. They are never cached, shared, bundled, published
  or fetched outside it (`docs/toolchains.md`, `docs/build.md`).
* **Manifest semantics.** `Yam.toml` describes a **project** (the `[project]` table).
  Targets follow Cargo-style conventions, with `sources`/`exclude` glob overrides for
  porting existing code. `std` is a *minimum* (`c++11` to `c++26`). Each project
  compiles all of its own files at `max(own std, declared std of each direct
  dependency)`; there's no whole-graph or per-file standard. `[dependencies]` is
  parsed but reserved until M4. See roadmap #10 and `docs/build.md`.

### Performance Discipline (strict)

**Targets.** The Linux release build is **static musl + mimalloc** (x86_64 and aarch64,
#8), so every performance change is measured on it first. Dynamic glibc, macOS and
Windows are first-class too: a change must not regress them. When a platform can't
be measured here (macOS and Windows, today), say so in the PR, and keep the change
platform-neutral (`std` or a crate, never `cfg`-gated tricks) so CI covers it.

**Measure, don't assume.**
* Every performance claim in a PR, doc or reply comes from a measurement made for it,
  with the command that made it. Anything not measured is written as "not measured".
* Compare binaries side by side: keep the old binary, and time old and new for
  **both C libraries** (glibc and musl) in the **same** `hyperfine -N` invocation
  (`-w 10`, at least 40–50 runs), never across sessions. Wait until the machine is
  idle (1-minute load below 2) and nothing else is building.
* Report the **median of at least three rounds**, with user and system time as well
  as wall time. Differences within about 0.5 ms or one σ are noise; don't report a
  single round as a result, and don't call two builds equal or one faster from one
  run.
* Cover **both styles** (modules and legacy) at 1k units at least, and say which
  scenarios and sizes were measured. Compare against the `ninja` baseline in
  `docs/bench.md`.
* **Isolate each change.** When two changes land together, measure each alone and
  together before crediting either. An interaction can hide a gain or invent one
  (the shared buffers only paid off with huge pages off).
* A state-format change invalidates old state: benchmark the new binary on its own
  copy of each project, at a path of the same length, built once before timing.

**Profile before optimizing.**
* Profile the static musl binary with `samply record --save-only
  --unstable-presymbolicate -r 4000 --iteration-count 100`, built with
  `CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`.
  Look at the main thread separately from worker threads: the main thread is the
  wall-clock path.
* Count instead of guessing: `strace -c -f` for system calls, `/usr/bin/time -f %M`
  for peak memory, and mimalloc's `MIMALLOC_SHOW_STATS=1`. Check memory when system
  time moves without a code reason.

**Prove a performance change is still correct.**
* Anything touching staleness, hashing, scanning or the state file runs the edit
  scenarios (`touch`, `leaf-impl`, `leaf-iface`, `impl-1pct`, `mid-iface`,
  `add-unit`) with the old and new binaries on fresh copies, in both styles. The
  compile counts must match, and the build after each must compile nothing.
* A command hash keeps the rule "a different command is a different hash". Say in
  the PR when a change forces one full rebuild (a state-format version bump or new
  hash inputs).

**Code patterns for the hot path.** musl's `memcpy` and `memcmp` are slow for small
sizes, so per-file small work costs far more there than on glibc. These patterns help
every platform:
* No per-file or per-input path joins, `format!`, `String` keys or small copies on
  the no-op path. Intern paths and names once (`StrTable`, path ids, shared
  `Arc<str>`), key records by id, and build a `Command` only for a step that runs.
* Hash what many commands share once (a cloned `CommandHasher`) and add only the
  per-file part; don't hash what's already compared as inputs.
* Few large allocations, not many small ones: one buffer, sized once up front (a
  growing `Vec` moves into fresh memory each time it grows).
* Worker threads on the no-op path don't allocate, so the allocator never sets up a
  heap for them. They write into memory allocated before they start.
* Keep mimalloc's `no_thp` feature. Never change a process-wide setting that child
  processes inherit (such as `prctl(PR_SET_THP_DISABLE)`): it would reach the
  compilers yam runs.
* Overlap independent work (`stat` prefetch during the manifest read and source walk)
  rather than adding threads to the critical path.
* Replacing libc functions (musl's `memcpy` and friends) is out: it needs `unsafe`
  or C. Revisit only if #28 shows the musl gap growing with project size.

**Write it down.** Record each result in `docs/bench.md` as a before/after table
(glibc and musl, with the `ninja` column) plus what was tried and dropped, and update
the design in `docs/build.md`. When a new measurement contradicts an earlier claim,
correct the claim in the same change.

### Terminology (strict)

Use the terms in `docs/glossary.md` consistently in code, docs, issues and CLI
output:
* **project**: the source tree with a `Yam.toml`; what you build.
* **target**: a project's library or binary.
* **package**: the published artifact that `yam publish` produces from a project;
  what others depend on.
* **build graph**: the root project plus all of its dependencies.

The crate name `yet-another-make` uses Cargo's sense of "package"; that's unrelated.

### Design Docs (refer to these strictly)

* `docs/roadmap.md`: the plan of record. Issue `#N` there is GitHub issue `#N`.
* `docs/glossary.md`: project, target, package, dependency, build graph, workspace.
* `docs/build.md`: the build engine (scanning, staleness, state file, scheduler,
  performance gates).
* `docs/release.md`: distribution layouts and the two-tier path resolution (user space
  first, then relative to the binary).
* `docs/packages.md`: the OCI/GHCR package model. Don't invent resolution or registry
  behavior that the doc still lists as open.
* `docs/toolchains.md`: the `yam-toolchain` split, toolchain pinning, and libc++ per
  target triplet.
* `docs/modules.md`: `yam-iface` interface extraction at publish time.

### Workflow

* Work on one issue per branch, branched from `main`, and open a PR that says
  `Closes #N`. Don't stack PRs: merging a stacked series with branch deletion closed a
  PR once.
* The repo auto-deletes merged branches. The maintainer reviews and merges PRs unless
  they ask otherwise.
* Never publish to crates.io, push tags, or change repo settings without explicit
  approval.
