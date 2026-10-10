# YAM roadmap

`yam` (crate `yet-another-make`) is a simple, Cargo-style build tool for modern C++
whose headline goal is to **out-perform CMake + ninja**. It is written in Rust. An
earlier C++ prototype serves as the behavioral spec for M1. Its design docs
(`release.md`, `packages.md`, `toolchains.md` and `modules.md`) carry forward into
this repo's `docs/`. Terms (project, target, package, dependency, build graph) are
defined in [glossary.md](glossary.md).

Issues are numbered in creation order, so on a fresh GitHub repo `#N` here matches
GitHub issue `#N`.

## Settled decisions

- **Rust, single published crate** `yet-another-make` that installs a binary named
  `yam`.
  - A crates.io package can't depend on unpublished path crates, so the internal
    structure is a module hierarchy (`cli/`, `manifest/`, `engine/`, `toolchain/`)
    inside one crate.
  - `bench/` is a workspace member with `publish = false`.
- **Never link LLVM.**
  - Module dependencies come from **batched `clang-scan-deps -format=p1689`**, plus a
    **persistent scan cache** keyed on file content and flags.
  - Header dependencies come from `-MD` depfiles written during compilation.
  - A native fast-path scanner is added only if benchmarks justify it.
- **Performance is a measured gate.** The benchmark harness lands in M1, and M2
  exits only when yam beats ninja (#28).
- **Targets use Cargo-style conventions**, with glob overrides for porting existing
  code (#10).
- **`std` is a minimum, and each project compiles at one standard**, raised only by
  its direct dependencies (#10).
- **Compatibility guarantee:** a lib built and published by yam works when linked
  into any other yam build on the same toolchain line. Prebuilt and closed-source
  binaries are first-class (#33).
- **Allocator: mimalloc on every target.** Official Linux release binaries are
  static musl + mimalloc; `cargo install` builds natively and still gets mimalloc
  (#8, #27).
- **Portability rule:** use Rust `std` first, then a well-maintained crate, and raw
  OS calls only as a last resort.

## Repo shape (target)

```
yet-another-make/
├── Cargo.toml              # workspace + package `yet-another-make`, [[bin]] name = "yam"
├── src/
│   ├── main.rs             # yam binary entry (thin)
│   ├── lib.rs              # yet_another_make library root
│   ├── cli/                # clap derive, subcommands (init, build, clean)
│   ├── manifest/           # Yam.toml model, parse/validate/render (serde + toml)
│   ├── engine/             # graph, scan cache, scheduler, depfiles, state file
│   └── toolchain/          # locate pinned clang / clang-scan-deps / libc++ std.cppm
├── bench/                  # publish = false: project generator + hyperfine scripts
├── docs/
└── tests/                  # CLI integration tests (assert_cmd, insta snapshots)
```

**Crates:**
- `clap`, `serde`, `toml`
- `thiserror` in library code, `anyhow` at the binary edge
- `anstream`/`anstyle` for color
- `git2`, already in use
- `blake3`, `globset`, `which`
- dev and bench only: `tempfile`, `assert_cmd`, `insta`, `criterion`

## Labels

`area:cli` `area:manifest` `area:engine` `area:toolchain` `area:packages` `area:bench`
`area:infra` `area:docs` · `type:feature` `type:perf` `type:design` · `blocked`

---

## M0 — Foundation

1. **Restructure skeleton into lib + bin + workspace** (infra)
   - Already in place: the package, `[[bin]] yam`, edition 2024,
     `MIT OR Apache-2.0`, and the release profile (LTO, `codegen-units = 1`,
     `panic = "abort"`, strip).
   - Add `src/lib.rs` and move `cli`, `style` and `commands` under it. `main.rs`
     becomes a thin wrapper.
   - Add a `bench` workspace member with `publish = false`.
   - Add `[workspace.lints]`: clippy `pedantic` warn and `unsafe_code` deny, with an
     allow for the mimalloc global allocator if needed.
   - Fill in the missing crates.io metadata: `description`, `repository`, `readme`,
     `keywords`, `categories`.
   - Move the forced `target = "x86_64-unknown-linux-musl"` out of
     `.cargo/config.toml`, since it breaks macOS and Windows builds. The musl static
     build plus mimalloc moves into the Linux release job (#8).
   - `cargo build`, `cargo test` (all existing `init` tests) and `cargo package` pass
     on the default host target.
2. **rustfmt + clippy config** (infra)
   - Add `rustfmt.toml`.
   - `cargo clippy --all-targets -- -D warnings` is clean.
3. **CI matrix: Linux, macOS, Windows** (infra)
   - GitHub Actions runs fmt check, clippy and tests on all three OSes.
   - Clang 22 is installed on the runners for later integration tests.
4. **Port design docs** (docs)
   - Copy `release.md`, `packages.md`, `toolchains.md` and `modules.md` from the C++
     prototype.
   - Fix references that only make sense for the C++ implementation.
5. **`docs/build.md`: build engine design** (docs, design)
   - Record the scanning decisions: no LLVM linking, batched P1689 scans, scan cache,
     `-MD` depfiles, fast-path scanner deferred.
   - Record the build state file format and the perf goals and gates.
6. **CLAUDE.md for the Rust repo** (docs)
   - Commands, the zsh `-ic` shell rule, the portability rule and doc pointers.
   - Rust idioms replace the clang-tidy/clang-format contracts.
   - Merge useful entries from the existing `.claude/settings.local.json`.
7. **Claim the crates.io name: publish `yet-another-make` 0.0.1** (infra)
   - Publish a real, minimal release: a working `yam --version` and `yam init`, and a
     README that states the project's status.
   - Published early so the name can't be taken mid-development.
   - Done manually with `cargo publish`, only after explicit approval.
8. **Release automation: crates.io + GitHub Releases** (infra)
   - On a `v*` tag, publish to crates.io via **Trusted Publishing** (GitHub OIDC, with
     no long-lived token). Once it works, revoke the manual 0.0.1 publish token.
   - Attach prebuilt `yam` binaries to a GitHub Release, using `cargo-dist` or an
     equivalent:
     - **Linux: musl only** (`x86_64-` and `aarch64-unknown-linux-musl`), static and
       using mimalloc. No `*-linux-gnu` binaries are published.
     - macOS (`aarch64-apple-darwin`) and Windows (`x86_64-pc-windows-msvc`).
   - Release CI checks that every Linux artifact is static and links mimalloc.
   - A CI job on an Ubuntu (glibc) runner checks that `cargo binstall
     yet-another-make` installs the **musl** binary (binstall's fallback when there's
     no gnu artifact). binstall metadata comes from cargo-dist or
     `[package.metadata.binstall]`.
   - `cargo install` stays a native host build: building for musl needs a musl C
     toolchain for `libgit2-sys` and `mimalloc`. The README documents `cargo binstall`
     (recommended), `cargo install`, and the opt-in
     `cargo install --target x86_64-unknown-linux-musl yet-another-make`.
9. **Reconcile `cargo install` with `docs/release.md`** (docs, design)
   - `cargo install` places `yam` in `~/.cargo/bin`, with no bundled layout alongside
     it.
   - Define how two-tier path resolution behaves there. Tier 1 (user space:
     `$HOME/.yam` / `%LOCALAPPDATA%\YAM`) works as usual. Tier 2 (relative to the
     binary) finds nothing, so the default toolchain and target must be installed into
     user space on first use instead of being bundled.
   - Document crates.io as a distribution channel alongside RPM, DEB, Snap, Windows
     and macOS.

## M1 — Parity with the C++ prototype (+ benchmark harness)

Acceptance criteria quote the C++ prototype's behavior, which becomes the first tests.

10. **`Yam.toml` model + parsing** (manifest)
    - `[project]` has `name` (required), `version` (default `0.1.0`) and `std`.
    - **`std` is a minimum**, like Cargo's `rust-version`: it's the lowest C++
      standard the project's own code and public interface need.
      - Allowed values: `c++11` to `c++26`. C++98/03 are deliberately unsupported.
      - `yam init` writes `c++26` for a bin and `c++23` for a lib (the lowest that
        supports `import std`). `--legacy` may write lower.
    - **One standard per project:** all of a project's own files compile at
      `project_std = max(own std, declared std of each direct dependency)`.
      - A `c++20` app that depends on a `c++26` module library builds at c++26, with
        a `note:` naming the dependency that raised it.
      - A `c++11` library used by a `c++26` app still compiles its own files at
        c++11; only the app is at c++26.
      - Indirect dependencies don't raise it. A dependency's declared `std` is a
        promise about its public interface, checked at publish time (#33).
      - It's an error only if `project_std` exceeds what the pinned toolchain
        supports.
      - The rationale and examples are in `docs/build.md`.
    - **Cargo-style target conventions.** No target table is needed:
      - `src/main.cpp` gives a bin named after the project.
      - `src/lib.cppm` (or `src/lib.cpp`) gives a lib.
      - Each `src/bin/<name>.cpp` gives an extra bin.
    - Default source sets:
      - The lib gets everything under `src/` except `main.cpp` and `src/bin/`.
      - Each bin gets its entry file, and links the lib if one exists.
    - **Optional `[lib]` and `[[bin]]` tables, for porting existing code:**
      - `name`, `path` (entry file)
      - `sources` (a list of globs, e.g. `["lib/**/*.cpp", "lib/**/*.cppm"]`)
      - `exclude` (globs)
      - `include-dirs`
      - When `sources` is set, it replaces the conventional source set entirely.
    - `[dependencies.<name>]` accepts `version`, `path`, `git` and `rev`. It's parsed
      but **reserved** (no effect) until M4.
    - Error to match: `missing required field 'project.name'`.
11. **Manifest validation diagnostics** (manifest)
    - Errors for an empty name or version, or an invalid project or target name (an
      ASCII letter or `_`, then letters, digits, `-` or `_`; `yam init` shares the
      rule). An invalid `std` is already rejected at parse time.
    - **Everything stays inside the project root:** `path`, `sources`/`exclude` globs
      and `include-dirs` must be relative, with no `..`. Code elsewhere is reached
      through a path dependency.
    - A target's entry file must exist and be part of its own source set.
    - Malformed globs are errors. A user-written glob that matches nothing is a
      warning; the conventional library globs may legitimately match nothing.
    - A missing `include-dirs` directory is a warning.
    - **A file in both the library and a bin is an error.** The bin already links the
      library, so the file would be compiled and linked twice. Two bins may share a
      file.
    - Duplicate `[[bin]]` names are an error.
    - A dependency with none of `version`, `path` or `git` is an error. `rev` without
      `git` is a warning.
    - Unknown manifest keys are warnings (e.g. `unused manifest key 'lib.inlcude-dirs'`).
    - Each diagnostic carries a severity, a key path, a message and an optional
      `help:` hint. Validation reports every problem in one run.
12. **Manifest render + round-trip** (manifest)
    - Rendering then re-parsing gives the same values (property test).
13. **CLI output helpers: `warning:` / `help:` / `note:`** (cli)
    - Clap, `--color` and the `status`/`error` helpers already exist.
    - Add `warning`, `help` and `note`, used by diagnostics (#11), `init` hints and the
      std-raise note (#10).
14. **Align `yam init` with the manifest model and modules-first templates** (cli)
    - Keep:
      - `[PATH]`, `--bin`/`--lib`, `--name`, `--vcs <git|none>` and `--legacy`
      - VCS auto-detection that skips init inside an existing repo
      - the name rule
      - appending `/target` to `.gitignore`
      - the `Created …` status line
      - all existing tests
    - Changes:
      - Write `Yam.toml` through the manifest module (#10, #12) instead of
        `format!`.
      - Write `std` per #10. The current `"c++20"` default is wrong, because
        `import std` needs at least C++23.
      - With the Cargo-style defaults, no target table is written.
      - The `--lib` template becomes `src/lib.cppm` with `export module <name>;`.
      - `--lib --legacy` gives `src/lib.cpp` plus `include/<name>.hpp`.
      - Bin templates use `std::println`.
      - Messages say "project": `Created binary (application) project` and
        "cannot be run on existing yam projects".
15. **`yam clean`** (cli)
    - Arguments: `[PATH]`, `--release` (release artifacts only), `--dry-run` and
      `-v/--verbose`.
    - Removes `target/` or `target/Release`.
    - Reports what was removed, or would be with `--dry-run`.
16. **Toolchain discovery** (toolchain)
    - Locate `clang++` and `clang-scan-deps`: env override, then pinned path, then
      `PATH`.
    - Locate `std.cppm` via `clang++ -print-file-name=libc++.modules.json`, with no
      hardcoded `/usr/lib/llvm-22`.
    - Missing tools produce a clear, actionable error.
17. **`yam build`, single-file target (parity)** (engine)
    - Arguments: `[PATH]`, `--release`, `-v/--verbose` and `--target <NAME>`.
    - Builds into `target/Debug` or `target/Release`, with `-O0 -g` or
      `-O3 -DNDEBUG`.
    - Builds `std.pcm` on every build, per `docs/toolchains.md`.
    - A bin produces an executable; a lib produces a `.a` (via `llvm-ar`, falling
      back to `ar`).
    - Prints `Building <name> v<ver> (<dir>)` and `Finished …`.
    - A non-empty `[dependencies]` gives a warning that they're ignored.
18. **Benchmark project generator** (bench)
    - `cargo run -p bench -- gen --modules N --fanout F --headers H` emits two
      equivalent builds of the same project: a `Yam.toml` project and a CMake + ninja
      project.
    - Sizes: 100, 1k and 10k modules.
19. **Benchmark runner + baseline** (bench, perf)
    - `hyperfine` scripts for cold, no-op, one-leaf-edit and one-root-interface-edit
      builds.
    - Compare against `cmake --build` (ninja) and `ninja` alone.
    - Record the C++ prototype and CMake + ninja baselines in `docs/bench.md`.

## M2 — Build engine (the performance milestone)

20. **Multi-file, multi-target builds** (engine)
    - Build the full #10 target model:
      - the conventional source sets and `src/bin/*`
      - `sources`/`exclude` globs
      - `include-dirs`
      - bins linking the project's lib
    - Compile each project at its project standard (#10), including local-project
      dependencies built from source.
    - Add `links` and `link-dirs` on `[lib]`/`[[bin]]`, for system and vendor
      (closed-source) libraries, e.g. `pthread`, `z` or a vendor `.a`. This is a
      manual escape hatch until M4 packages exist.
    - Expand globs with `globset` and a directory walk. Cache directory listings and
      mtimes in the state file (#26), so no-op builds don't re-walk unchanged trees.
    - Two `main()` entry files in one bin is a clear error.
    - **Porting acceptance test:** an existing open-source, header-based C++ library
      plus its CLI builds from a `Yam.toml` that uses only `sources`, `exclude`,
      `include-dirs` and `links`, with no changes to its source files.
21. **Batched dependency scanning** (engine)
    - One `clang-scan-deps -format=p1689` call per build, over a generated compilation
      database.
    - Parse P1689 into provides/requires edges.
22. **Persistent scan cache** (engine, perf)
    - Keyed on `blake3(content)` plus a flags hash.
    - Only new or changed files are rescanned; a no-op build performs **zero** scans.
    - Stored in `target/<Profile>/.yam/`.
23. **Header deps from depfiles** (engine)
    - Compile with `-MD -MF`.
    - Ingest depfiles into the state file after each compile, the way `.ninja_deps`
      works.
24. **Build graph + staleness** (engine)
    - Graph of modules, objects, `std.pcm` and link steps, with one `std.pcm` per
      distinct project standard that uses `import std`.
    - Staleness from mtime with a content-hash fallback, plus a command-line hash.
    - Cycles are reported with the module chain.
25. **Parallel scheduler** (engine, perf)
    - Topological execution, `-j` defaulting to the number of CPUs.
    - Keep going on failure; output is grouped per job.
    - Optional make jobserver client (`jobserver` crate).
26. **Binary build state file** (engine, perf)
    - One compact file, loaded with a single read at startup.
    - Versioned header; a corrupt file triggers a rebuild, not a crash.
27. **Startup & stat hot-path profiling** (perf)
    - Profile no-op builds with `perf` and `cargo flamegraph`.
    - Remove allocations from the per-file check loop; parallelize `stat` calls if it
      helps.
    - Confirm that mimalloc beats the platform allocator on glibc, macOS and Windows.
      It's enabled on all targets; drop it per target if the data says otherwise.
28. **Perf gate: beat ninja** (bench, perf)
    - On 100, 1k and 10k module projects:
      - no-op `yam build` ≤ `ninja` no-op
      - leaf edit ≤ `cmake --build`
      - root edit ≤ `cmake --build`
    - Results are recorded in `docs/bench.md`. **This is M2's exit criterion.**
29. **Native fast-path import scanner (spike)** (engine, perf, design)
    - Only if #28 shows scanning matters.
    - Lex the module preamble directly, and fall back to `clang-scan-deps` whenever
      `#if` or macros are present.
    - A test suite cross-checks it against `clang-scan-deps`.

## M3 — Toolchain management

30. **`yam toolchain install <version>` / `yam toolchain update`** (toolchain)
    - `blocked` on the versioning/tagging scheme in `docs/toolchains.md`. Settle that
      before M3 starts.
31. **Local libc++ build for an uncached target triplet** (toolchain)
    - Happens transparently on first use, per `docs/toolchains.md`.
32. **Two-tier path resolution** (toolchain)
    - User space first (`$HOME/.yam` / `%LOCALAPPDATA%\YAM`), then the bundled
      layout relative to the `yam` binary, per `docs/release.md`.
    - Includes the `cargo install` case from #9.

## M4 — Packages (epic, `blocked` on design)

Start the #33 design work alongside M2, so it is ready when #28 passes.

33. **Design: resolve `docs/packages.md` open questions** (packages, design)
    - Covers media type, naming/tags, version ranges, auth, cache, resolver and
      `Yam.lock`. Implementation issues get filed once this closes.
    - **Requirement — compatibility guarantee:** a lib built and published by yam
      works when linked into any other yam build on the same toolchain line.
      Mechanisms already decided:
      - `std` is a minimum, and each project compiles at
        `max(own std, direct dependencies' declared std)` (#10).
      - `yam publish` turns a **project** into a **package**. A dependency resolves to
        a package from a registry or to a local project built from source.
      - Packages ship interface *source*, never a compiled module interface.
      - At publish time, `yam-iface` compiles the extracted interface at the declared
        minimum `std`, so the minimum is proven, not just claimed.
      - **Prebuilt binaries are the primary path, because closed-source libs must
        work.**
        - A package ships its declarations-only interface (`.cppm` from `yam-iface`,
          or headers for legacy code) plus prebuilt `.a`/`.so` for each target
          triplet, as one OCI image index per package.
        - The consumer compiles only the interface, at its own project standard, and links the
          prebuilt binary. This relies on libc++ keeping its ABI stable across `-std`
          modes.
        - Rebuilding from source is an **optional fallback**, only when the package
          includes source and the publisher allows it (for example, a triplet that
          wasn't prebuilt).
      - **ABI fingerprint, checked at resolve time, not link time:** each prebuilt
        binary records:
        - target triplet
        - toolchain major version
        - libc++ ABI version
        - exceptions and RTTI on or off
        - PIC
        - sanitizers
        - `_LIBCPP_ABI_*` overrides

        A mismatch is a clear error, for example "foo 1.2 was built for libc++ ABI
        v1 / clang 22; this build uses …".
      - **ODR guard:** exported inline and template bodies are compiled in the
        consumer at the consumer's project standard. At publish, `yam-iface` warns when they depend
        on `__cplusplus` or feature-test macros.
      - Debug and release consumers can link the same release binary, since libc++
        hardening modes don't change the ABI. Publishers may also ship a debug
        build.

## M5 — Extensibility

34. **`yam-<verb>` PATH dispatch** (cli)
    - Unknown subcommands run `yam-<verb>` with the remaining arguments, like Cargo
      and Git.
35. **`yam iface` preview** (cli)
    - Dispatches to `yam-iface` from the pinned toolchain.
