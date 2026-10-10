# Build Engine Design

## Overview

`yam build` turns a project's sources into binaries and libraries. It talks to the
pinned Clang toolchain directly. There is no CMake or ninja underneath, and no
intermediate build file. The engine has one headline requirement: **out-perform
CMake + ninja**, measured rather than claimed (see [Performance gates](#performance-gates)).

Sections marked **Decided** are settled and implemented against. Sections marked
**Proposed** are the starting design for the issue named. They may change while that
issue is implemented, and this doc is updated in the same PR.

Issue numbers (`#N`) refer to [roadmap.md](roadmap.md) and the matching GitHub issues.
Terms (project, target, package, dependency, build graph) are defined in
[glossary.md](glossary.md).

## Where the time goes

Compiling dominates a full build. Developers mostly run **no-op** and **small edit**
builds, though, and those are dominated by the build tool's own overhead. ninja is
fast here because it:

- starts quickly and parses its build file fast
- checks file timestamps cheaply
- loads dependency information from compact binary logs (`.ninja_deps`,
  `.ninja_log`) instead of rediscovering it
- spawns compiler processes with minimal overhead

yam has to match all of that. It also has one structural advantage: it owns the whole
pipeline. CMake + ninja handles C++ modules in two phases. CMake generates the build
files, then ninja runs a module dependency scan before compiling. yam can keep scan
results between runs and rescan only what changed, which is its main lever for beating
CMake + ninja on modules-heavy projects.

## Pipeline

```
Yam.toml ─► target model ─► source discovery ─► toolchain ─► std.pcm
                                                              │
   link ◄── scheduler ◄── staleness ◄── build graph ◄── dependency scan
```

1. **Load the manifest** and resolve the target model (#10): Cargo-style conventions,
   or `[lib]`/`[[bin]]` overrides with `sources`/`exclude` globs.
2. **Discover sources** by expanding globs. Directory listings and their mtimes are
   kept in the state file, so unchanged directories aren't walked again (#20).
3. **Locate the toolchain**: `clang++`, `clang-scan-deps` and `std.cppm` (#16). Each
   tool comes from an override (`YAM_CXX`, `YAM_CLANG_SCAN_DEPS`), then the bundled
   toolchain, then `PATH`; `clang-scan-deps` is first looked for next to `clang++`.
   `std.cppm` comes from `clang++ -print-file-name=libc++.modules.json`.
4. **Compute each project's standard** (see
   [One standard per project](#one-standard-per-project--decided)).
5. **Ensure a `std.pcm`** for each distinct standard in use (see [std.pcm](#stdpcm--decided)).
6. **Scan** new and changed files for module provides/requires (#21, #22).
7. **Build the graph** of module interfaces, objects, `std.pcm` and link steps (#24).
8. **Decide staleness** for each node (#24).
9. **Schedule** stale nodes in parallel, in dependency order (#25).
10. **Link** binaries and archive libraries.
11. **Persist** the updated state file (#26).

A no-op build must finish steps 1–8 and find nothing to do, without spawning any
process.

## Dependency discovery — Decided

- **Never link LLVM into yam.**
  - It would lock the `yam` binary to one Clang version, which breaks toolchain pinning
    and `yam toolchain install`. That's the same reason `yam-iface` is a separate
    tool; see [modules.md](modules.md).
  - It would add tens of megabytes and slower startup.
  - Clang's dependency-scanning library is C++-only, so using it from Rust would mean a
    painful C++ bridge.
- **Module dependencies come from `clang-scan-deps -format=p1689`**, run **once per
  build** over a generated compilation database that covers every file needing a
  scan. Batching pays the process start-up cost once and lets `clang-scan-deps` share
  its own internal file cache across files (#21).
- **The scan cache persists between builds** (#22).
  - Each file's P1689 result is keyed on `blake3(file contents)` plus a hash of the
    flags that affect scanning (the project's standard, defines, include dirs, triplet).
  - Only new or changed files are rescanned.
  - **A no-op build performs zero scans.**
- **Header dependencies come from depfiles.** Each compile runs with `-MD -MF`, and
  yam reads the depfile into the state file afterwards, the way ninja maintains
  `.ninja_deps` (#23). Headers never need scanning before a compile, because a
  file's first compile is unconditional anyway.
- **A native fast-path scanner is deferred** (#29). C++20 restricts module
  declarations at the top of a file enough that, when no `#if` or macros surround
  them, yam could read them directly. That is built only if #28 shows scanning still
  matters after caching. If built, it falls back to `clang-scan-deps` whenever the
  file's top section uses the preprocessor, and a test suite cross-checks it against
  `clang-scan-deps`.

## One standard per project — Decided

`std` in `Yam.toml` is a **minimum** (like Cargo's `rust-version`): the lowest standard
the project's own code and its public interface need. Allowed values are `c++11` to
`c++26`; C++98/03 are not supported.

Every file in a project compiles at one standard:

```
project_std = max(project's own std, declared std of each direct dependency)
```

- When a dependency raises it, yam prints a `note:` naming the dependency.
- **Indirect dependencies don't raise it.** A dependency's declared `std` is a promise
  about its public interface. At publish time `yam-iface` compiles that interface at
  the declared minimum, which keeps the promise honest. A project that re-exports a
  C++26 module can't publish with a lower `std`.
- It's an error only if `project_std` is above what the pinned toolchain supports.
- Linking projects compiled at different standards is safe: libc++ keeps its ABI
  stable across `-std` modes, and the package ABI fingerprint (#33) covers everything
  else.

**Why per project.**
- **Old code isn't forced onto new standards.** Compiling the whole build graph at
  its highest `std` would push a C++11 dependency to C++26, and that fails on removed
  features: `std::auto_ptr`, `throw(X)`, `register`, `bind1st`, `u8` literals and
  comparison-operator rewrites.
- **No mixed standards inside one binary.** Per-file standards would let one
  project's internal headers, inline functions and templates compile differently in
  different files, which is a silent ODR violation.
- **The standard changes only when `Yam.toml` does.** Adding one `import` never
  quietly changes a file's standard.
- **The module guarantee holds.** A file that imports a module always compiles at or
  above that module's project standard, which Clang requires when it loads a compiled
  module interface.

**Examples.**

| Build graph | Standards used |
|---|---|
| C++26 app → C++11 header-based library | The library's own files compile at C++11. The app compiles at C++26 and includes the library's headers at C++26. |
| C++11 app → C++26 module library | The app is raised to C++26, with a `note:` naming the library. |
| C++26 app → B (C++17) → C (C++11) | The app is at C++26, B at C++17 and C at C++11. Each is raised only by its own direct dependencies. |
| A C++11 project needs a C++26 module in one file | The whole project is raised. If that's unwanted, split it into two projects. |

## std.pcm — Decided

`std.pcm`/`std.compat.pcm` are **built locally into the build directory, and never
cached, shared, bundled, published or fetched** outside it (see
[toolchains.md](toolchains.md)).

A build produces one `std.pcm` for each distinct project standard that uses
`import std` (C++23 and later). Most builds have exactly one. Each extra one costs
about 2s, and only once.

Within `target/<Profile>/`, each `std.pcm` is an **ordinary staleness-tracked
artifact**. It is rebuilt only when one of these changes:

- the identity of the clang binary (path, size, mtime, and its `--version` output)
- its full compile command, including flags, the standard and the target triplet
- `std.cppm`

Recompiling it on every invocation would put a ~2s floor under every no-op build and
make the no-op gate in #28 impossible.

## Staleness — Proposed (#24)

An output is stale if any of these hold:

- it's missing
- its **command hash** differs from the recorded one (the hash covers the full argv,
  the compiler identity and the relevant environment)
- any recorded input (the source, depfile headers, imported module interfaces,
  `std.pcm`) has a newer mtime than recorded
- for an input whose mtime changed: its blake3 content hash differs from the recorded
  hash

The content-hash fallback turns `touch`, `git checkout` and editor save-without-change
into no-ops. Hashing happens only when an mtime has already moved, so the common path
is `stat` alone.

Like ninja's `restat`: when a module interface is recompiled but its output (`.pcm`)
comes out byte-identical, yam does not treat its importers as dirty.

## Build state file — Proposed (#26)

The state file is one file per profile, `target/<Profile>/.yam/state`, read with a
single read at startup and written at the end of the build.

- **Header:** magic number, format version, yam version and a checksum. A version
  mismatch or a corrupt file is discarded with a `note:`, and the build runs as a
  clean build. Never a crash.
- **Path table:** interned paths, so every other table refers to a path by an integer.
- **Tables:**
  - files: mtime, size and blake3 per path
  - directory listings: mtime plus the entries, for glob expansion (#20)
  - scan results: module provides/requires keyed on content and flags hash (#22)
  - header dependencies from depfiles (#23)
  - outputs: the command hash and the input set each output was built from (#24)
- **Durability:** written to a temporary file and atomically renamed into place.
  Results are recorded as jobs finish, so an interrupted or failing build keeps the
  progress it made.
- **Encoding:** a compact binary layout designed for zero-copy or near-zero-copy
  loading. The exact crate (hand-rolled, `rkyv`, `bincode`, …) is chosen in #26 based
  on startup measurements.

## Scheduler — Proposed (#25)

- Jobs run in dependency order, with `-j` defaulting to the number of available CPUs.
- **Keep going on failure:** independent jobs continue after a failure, and the build
  fails at the end with every error reported.
- Each job's output is captured and printed as one block, so compiler diagnostics
  aren't interleaved.
- Compiler processes are spawned directly with `std::process`, never through a shell.
- Optional make jobserver client (`jobserver` crate), so yam shares CPU slots when
  it's run from inside another build.

## Build directory layout — Proposed

```
target/
└── <Profile>/                # Debug | Release
    ├── .yam/
    │   └── state             # build state file (#26)
    ├── std/<std>/            # std.pcm + std.o per standard in use, e.g. std/c++26/
    ├── modules/<project>/    # compiled module interfaces (.pcm)
    ├── obj/<project>/        # object files and depfiles, mirroring source paths
    ├── <bin>                 # the root project's linked executables
    └── lib<name>.a           # project libraries
```

## Performance gates

Measured with the #18/#19 harness. It generates equivalent `Yam.toml` and
CMake + ninja projects of 100, 1k and 10k modules and times them with `hyperfine`.

| Scenario | Gate (#28) |
|---|---|
| No-op build | `yam build` ≤ `ninja` alone (excluding CMake regeneration) |
| One leaf file edited | `yam build` ≤ `cmake --build` |
| One root module interface edited | `yam build` ≤ `cmake --build` |
| Cold build | Reported, not gated (compiling dominates) |

**M2 isn't done until every gate passes at every size.** Results and the machine they
ran on are recorded in `docs/bench.md`. #27 profiles the no-op path (`perf`,
`cargo flamegraph`) for allocations and `stat` cost, and checks that mimalloc beats
the platform allocator on each OS.

## Open questions

- **State file encoding:** the crate or a hand-rolled format, decided in #26 from
  startup measurements.
- **Parallel `stat`:** whether parallel `stat` calls on the no-op path beat a
  sequential walk on typical filesystems. Measured in #27.
- **`std.compat.pcm`:** built only when some file imports `std.compat`, or always.
- **File watching** (inotify or FSEvents, to skip the `stat` walk entirely): a
  possible later optimization. Out of scope until the gates in #28 pass without it.
