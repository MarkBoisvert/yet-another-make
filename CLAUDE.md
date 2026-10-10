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
  Proposed section changes.
* **Never link LLVM into `yam`.** Module dependencies come from batched
  `clang-scan-deps -format=p1689`, plus the persistent scan cache. `yam-iface` is a
  separate tool built by `yam-toolchain`.
* **`std.pcm`/`std.compat.pcm`** are built into the project's build directory and
  staleness-tracked like any object. They are never cached, shared, bundled, published
  or fetched outside it (`docs/toolchains.md`, `docs/build.md`).
* **Manifest semantics.** Targets follow Cargo-style conventions, with
  `sources`/`exclude` glob overrides for porting existing code. `std` is a *minimum*,
  and the whole graph builds at `effective_std = max(...)`. `[dependencies]` is parsed
  but reserved until M4. See roadmap #10.

### Design Docs (refer to these strictly)

* `docs/roadmap.md`: the plan of record. Issue `#N` there is GitHub issue `#N`.
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
