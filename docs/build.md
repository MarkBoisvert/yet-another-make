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
2. **Discover sources** by expanding globs over one walk of the project (#20).
   Directory listings and their mtimes are kept in the state file, so an unchanged
   directory isn't read again (#26).
3. **Locate the toolchain**: `clang++`, `clang-scan-deps` and `std.cppm` (#16). Each
   tool comes from an override (`YAM_CXX`, `YAM_CLANG_SCAN_DEPS`), then the bundled
   toolchain, then `PATH`; `clang-scan-deps` is first looked for next to `clang++`.
   `std.cppm` comes from `clang++ -print-file-name=libc++.modules.json`.
   Discovery spawns three processes, so its result is kept in the state file and
   reused while a fingerprint of what it looked at still matches (#27): the
   overrides, `PATH`, the mtime of every directory searched, and the stat of each
   tool and libc++ file found. Checking it is a few dozen `stat` calls.
4. **Compute each project's standard** (see
   [One standard per project](#one-standard-per-project--decided)).
5. **Ensure a `std.pcm`** for each distinct standard in use (see [std.pcm](#stdpcm--decided)).
6. **Scan** new and changed files for module provides/requires (#21, #22). One
   `clang-scan-deps -format=p1689` call covers every file whose scan isn't cached
   (`src/engine/scan.rs`, `src/engine/scan_cache.rs`).
7. **Build the graph** of module interfaces, objects, `std.pcm` and link steps (#24).
8. **Decide staleness** for each node (#24), as it's reached: a node runs only when
   its output is stale.
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
  - Each database entry carries the file's compile flags and its object file as
    `output`. P1689 reports that as the rule's `primary-output`, which is how rules
    are matched back to files; without `output` it's empty, and the rules come back
    in completion order.
  - A failed scan fails the build with the scanner's diagnostics, for example on a
    header-unit import, which yam doesn't support.
  - Whether a bin's file defines `main()` isn't a module dependency, so yam reads that
    from the text (`src/engine/entry.rs`).
- **The scan cache persists between builds** (#22).
  - Each file's P1689 result is keyed on `blake3(file contents)` plus a hash of the
    flags that affect scanning (the project's standard, defines, include dirs, triplet).
  - The flags hash also covers the scanner's and compiler's path and version. It
    doesn't cover the file's path, so identical files share a result.
  - Only new or changed files are rescanned.
  - **A no-op build performs zero scans** and starts no `clang-scan-deps` process.
  - The key also covers the contents of the project headers the file was last
    compiled with (from its depfile, #23). A header change that adds or removes an
    `import`, or flips a macro around one, rescans the files that include it.
- **Header dependencies come from depfiles.** Each compile runs with `-MMD -MF`, and
  yam reads the depfile into the state file afterwards, the way ninja maintains
  `.ninja_deps` (#23). Headers never need scanning before a compile, because a
  file's first compile is unconditional anyway.
  - `-MMD`, not `-MD`: only project headers are recorded. libc++ alone is over a
    thousand headers per file, and toolchain headers change only with the toolchain,
    whose path and version are already part of every key.
  - The record is part of the state file (#26), keyed by object file.
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

## Staleness — Decided (#24)

The build graph (`src/engine/graph.rs`) has a node for the `std` module, each compile,
the library's archive and each bin's link, and lists what each node needs. Nodes are
run in an order where every node follows what it needs; each one first checks the
output record (`src/engine/outputs.rs`) and is skipped when its output is fresh.

An output is stale if any of these hold:

- it's missing (for a module interface, its `.pcm` too)
- its **command hash** differs from the recorded one (the hash covers the full argv and
  the compiler's path and version)
- its set of inputs changed
- any input's blake3 contents hash differs from the one it was built from

Inputs are the source, the project headers from its last depfile (#23), and the
compiled interfaces it imports directly, including `std.pcm`; for an archive, its
objects; for a link, its objects, `std.o`, the library's archive and any library files
named in `links`.

- **Direct imports are enough** (#27). Clang records the signature of every module a
  `.pcm` imports inside the `.pcm`, so any change below a direct import, even an
  inline function body two modules down, changes that import's bytes as well. And an
  interface that comes out byte-identical means nothing below it changed. Listing
  every transitive interface instead made a 1k-unit modules no-op ~650 ms, since
  most files sit above most of the graph.

- **`stat` first.** Each input's mtime and size are compared with the last time yam
  saw the file; only when they moved is it read and hashed. So a no-op build is `stat`
  calls alone, and `touch`, `git checkout` and a save without changes rebuild nothing.
  The scan key's source and header hashes come from the same table, so the scan
  doesn't read unchanged files either.
- **Header paths are interned** (#65). The header record stores each header once and
  each object's headers as ids. A build resolves each header id once, to its id in
  the output record and a digest of its path and contents, so checking an object's
  headers compares integers and its scan key hashes one digest per header.
- **Inputs are recorded as they were checked,** before the step ran. A file edited
  while it was being compiled is seen as changed by the next build.
- **Restat, like ninja's:** an importer compares a `.pcm`'s contents, not its mtime, so
  a recompile that produces a byte-identical output leaves what depends on it fresh.
  The same holds for objects and archives. In Debug builds this rarely applies,
  because `-g` writes DWARF 5, which embeds each source file's checksum; any edit
  changes the object.
- **The record** is part of the state file (#26), with paths and hashes interned. A
  no-op build doesn't rewrite it, and a failed or killed build keeps the outputs it
  finished.
- `std.pcm`/`std.o` keep their own stamp (see [std.pcm](#stdpcm--decided)); what they
  feed is tracked by contents like any other input.

## Build state file — Decided (#26)

One file per profile, `target/<Profile>/.yam/state`, read with a single `fs::read`
before the source walk (`src/engine/state.rs`). It's a hand-rolled binary format: at
10k-unit scale (50k paths, 700k output inputs) it loaded in 11 ms, against 45 ms for
`postcard` and 87 ms for JSON, and needs no `unsafe` (which rules out memory-mapping).

- **Layout:** fixed-width little-endian fields.
  - header: magic `YAMSTATE`, format version, yam version
  - one table of every string; everything else refers to a string by index, so each
    path is stored once
  - sections, in order:
    - outputs: their paths and contents hashes (interned), what yam last saw of each
      file (mtime, size, hash), and each output's command hash and inputs (#24)
    - scan results, keyed on contents, headers and flags (#22)
    - header dependencies from depfiles, by object (#23)
    - directory listings: mtime plus entries, for the source walk (#20)
    - the discovered toolchain and its fingerprint (#27)
  - a blake3 checksum of everything before it
- **Errors:** a missing file is an empty state. A corrupt, truncated or other-version
  file is discarded with a `note:`, and the build starts over. Never a crash: every
  read is bounds-checked, and counts are checked against the bytes left.
- **Snapshot and journal, like ninja's logs:**
  - The snapshot is rewritten only when something changed, to a temporary file that
    is renamed into place. A no-op build writes nothing.
  - While a build runs, each finished job's results are appended to
    `.yam/state.journal` as framed records (length, payload, 8-byte blake3 check),
    with paths inline. The journal names the checksum of the snapshot it extends.
  - Loading replays the journal over that snapshot and stops at a half-written last
    record. A journal for another snapshot is ignored. So a build killed partway
    keeps the progress it made; the next save folds the journal in and removes it.
- **Directory listings:** a directory whose mtime hasn't moved isn't read again; its
  mtime changes whenever an entry is added, removed or renamed. A directory changed
  within the last two seconds isn't trusted next time, so an entry added within the
  same mtime tick can't be missed.

## Scheduler — Decided (#25)

- **Parallel:** up to `-j` nodes of the build graph run at once, each as soon as
  everything it needs is done. `-j` defaults to the number of available CPUs. Among
  ready nodes the earliest in graph order goes first, which puts module interfaces,
  which unblock the most, ahead of everything else.
- **One thread decides, workers run:** the main thread checks staleness, starts jobs
  and updates every record. Each job's commands run on a worker thread, spawned
  directly with `std::process`, never through a shell.
- **Grouped output:** each job's output is captured and printed as one block when it
  finishes, so compiler diagnostics aren't interleaved. Compiles ask Clang for color
  (`-fdiagnostics-color=always`), and printing strips it again when the output isn't
  a terminal or `--color never` is set.
- **Failure handling, like Cargo:**
  - **By default**, after the first failure yam starts no new jobs. Jobs already
    running finish, with their output and errors printed, while yam says
    `warning: build failed, waiting for other jobs to finish...`. Then the build
    fails. This matches `cargo build`.
  - **`yam build --keep-going`** keeps starting every job that doesn't depend on a
    failed one, skips the jobs downstream of a failure, and fails at the end with
    every error reported. This matches `cargo build --keep-going`.
  - Each distinct failure is reported once, with a count when several files of one
    target failed: ``could not compile `app` (bin): 2 files failed``.
  - Either way, outputs finished before the failure are recorded (#24), so the next
    build doesn't redo them.
- **No make jobserver client yet.** The `jobserver` crate's client constructor is
  `unsafe` (it takes ownership of inherited file descriptors), and yam denies
  `unsafe_code`. Until there's a safe way, run yam inside another build with an
  explicit `-j`.

## Build directory layout — Proposed

```
target/
└── <Profile>/                # Debug | Release
    ├── .yam/
    │   ├── state             # build state file (#26): outputs, scans, headers, listings
    │   └── state.journal     # results since the last snapshot, while a build runs
    ├── std/<std>/            # std.pcm + std.o per standard in use, e.g. std/c++26/
    ├── modules/<project>/    # the library's compiled module interfaces (.pcm)
    │   └── bin/<bin>/        # a bin's own module interfaces, if it has any
    ├── obj/<project>/
    │   ├── lib/              # the library's objects and depfiles, mirroring source paths
    │   └── bin/<bin>/        # each bin's objects, so bins may share a source file
    ├── rsp/                  # response files for long archive and link commands
    ├── scan/                 # the compilation database for clang-scan-deps (#21)
    ├── <bin>                 # the root project's linked executables
    └── lib<name>.a           # project libraries
```

Compiled interfaces are named the way Clang's `-fprebuilt-module-path` looks them
up: `m.pcm` for module `m`, and `m-part.pcm` for its partition `m:part`.

## Performance gates

Measured with the #18/#19 harness. It generates one source tree per size (100, 1k and
10k units) and style, containing both a `Yam.toml` and a `CMakeLists.txt`, and times
each scenario with `hyperfine`. There are two styles: **modules** (`import std`,
`.cppm` + `.cpp`) and **legacy** (`.hpp` + `.cpp`).

| Scenario | Change | Gate (#28) |
|---|---|---|
| `noop` | nothing | `yam build` ≤ `ninja` alone (excluding CMake regeneration) |
| `touch` | root interface mtime only | `yam build` ≤ `ninja` alone |
| `leaf-impl`, `leaf-iface` | one leaf unit | `yam build` ≤ `cmake --build` |
| `impl-1pct`, `impl-10pct` | function bodies in 1% / 10% of units | `yam build` ≤ `cmake --build` |
| `mid-iface`, `root-iface` | an interface with median / all dependents | `yam build` ≤ `cmake --build` |
| `add-unit` | a new leaf unit | `yam build` ≤ `cmake --build` |
| `cold` | clean build | Reported, not gated (compiling dominates) |

**M2 isn't done until every gate passes at every size, in both styles.** Results and the machine they
ran on are recorded in `docs/bench.md`, including #27's no-op profile and allocator
comparison.

## Open questions

- **Parallel `stat`:** #27 measured 5,000 `stat` calls (a 1k-unit modules no-op) at
  ~4–5 ms sequentially and ~1.5–2 ms on 4 threads, on ext4 under WSL2: about 10% of
  that no-op. Not done yet; #28 decides at 10k units whether it's worth the
  complexity.
- **`std.compat.pcm`:** built only when some file imports `std.compat`, or always.
- **File watching** (inotify or FSEvents, to skip the `stat` walk entirely): a
  possible later optimization. Out of scope until the gates in #28 pass without it.
