# Toolchain & Target Build Infrastructure

## Overview

Building and packaging the Clang toolchain and target-triplet sysroots that
`yam` bundles/downloads (see [release.md](release.md) for the on-disk layout
they end up in) is split out of this repo into **one** dedicated repo,
`yam-toolchain`, because it builds on a different cadence and process than
`yam` itself.

This used to be documented as a 3-repo split (`yam`, `yam-toolchain`,
`yam-targets`). It's 2 repos now. Target-triplet sysroot/artifact production
turned out to share the same underlying machinery `yam-toolchain` already
needs for cross-compiling itself (see "Why target artifacts live in
`yam-toolchain`" below) — there's no separate `yam-targets` repo, and no
plan to create one.

## What `yam-toolchain` builds

* **Host toolchain**: `clang`, `clang++`, `lld`, `lldb`, `lldb-server`,
  `clangd`, `clang-format`, `clang-tidy`, `clang-scan-deps`, and the
  associated Clang resource headers (`lib/clang/22/include/`). Compiling
  LLVM/Clang itself, one build per supported host platform (see "Platform
  tiers" below). Also includes `yam-iface` — `yam`'s own C++ module
  interface extraction tool, built against this same pinned LLVM version
  using LibTooling (not an upstream LLVM tool, but built and shipped
  alongside them for the same version-pinning reasons); see
  [modules.md](modules.md).
* **Target artifacts**: per target triplet (e.g. `x86_64-linux-glibc2.17`),
  a lightweight sysroot skeleton (headers, `crt/`, ABI-stub link data — see
  `yam-toolchain/docs/targets.md`) plus a compiled `libcxx/` bundle
  (`libc++.a`, `libc++abi.a`, `libunwind.a`) built against a specific
  pinned `yam-toolchain` version. One artifact per (toolchain version,
  target triple, build variant). **Never includes `std.pcm`/
  `std.compat.pcm`** — those are deliberately never precompiled, cached,
  or published anywhere; see "Resolved decisions" below.

Both are published as OCI artifacts to GHCR, using the same
"OCI artifact → registry → pull/verify/unpack" model as the package
registry described in [packages.md](packages.md) — one shared
artifact-fetch code path in `yam`, reused across user packages, toolchain
versions, and target artifacts.

## Why target artifacts live in `yam-toolchain`

Cross-compiling Clang/LLD/LLDB *for* a target platform that doesn't have
hosted native CI runners (see "Platform tiers") requires a sysroot to
cross-compile against — the same sysroot skeleton `yam` needs to
transparently build a target's `libcxx/` locally the first time a build
references a triplet that isn't already cached (see "Resolved decisions"
below — v1 has no explicit `yam target add` command). Rather than building
that sysroot-production capability twice — once inside `yam-toolchain` to
bootstrap its own cross-builds, once elsewhere to serve end users — it
lives in one place, and both consumers pull from it.

`yam` itself has no build-time dependency on producing sysroots or
compiling `libc++` from source as part of its own repo — it only *consumes*
`yam-toolchain`'s published artifacts (at its own release-build time, to
vendor a broad default skeleton set into the installer, and at runtime via
`yam toolchain update`), plus it contains the CLI-level logic to build
`libc++` locally, transparently, using an already-pinned toolchain the
first time a build references a target triplet whose `libcxx/` bundle
isn't already present — no cached artifact or network access required.
`std.pcm`/`std.compat.pcm` are a separate, unconditional case: `yam`
compiles those locally as part of *every* build, for every triplet,
regardless of whether `libcxx/` came from the base install, a local build,
or (later) an OCI fetch — see `yam-toolchain/docs/targets.md` for the full
design, measured build costs, and why.

## Platform tiers

`yam-toolchain` does not attempt to match Zig's full ~23-platform release
matrix (see `yam-toolchain/docs/platforms.md` for the complete tier
breakdown and rationale). Summary:

* **Tier 1** (native builds, hosted CI runners, full two-stage bootstrap if
  ever enabled): Linux x86_64/aarch64, macOS x86_64/aarch64, Windows
  x86_64/aarch64.
* **Tier 2** (cross-compiled on Tier-1 hosted runners + QEMU-tested, weaker
  validation confidence): Linux riscv64, evaluated case by case.
* **Tier 3** (Zig covers these; `yam-toolchain` does not, absent concrete
  demand): the BSDs, s390x, loongarch64, 32-bit arm.

## Resolved decisions

* **Build order**: `yam-toolchain`'s host toolchain build has no dependency
  on target artifacts. Target artifacts *do* depend on a specific published
  toolchain version (to compile `libc++` against it), so within
  `yam-toolchain`'s own pipeline, toolchain builds land before the target
  artifacts that reference them.
* **Bootstrap strategy**: start with a **single-stage build** (compile once
  with the build environment's system/native compiler — no PGO, no LTO
  bootstrap). This produces a fully correct, usable Clang; the traditional
  two-stage PGO+LTO bootstrap is a ~10-20% runtime-speed optimization on
  the shipped `clang` binary, not a correctness requirement, and it
  roughly doubles-to-triples build time/memory cost. Add it later as an
  opt-in once single-stage builds are running and the tradeoff can be
  measured for real. See `yam-toolchain/docs/architecture.md`.
* **`std.pcm`/`std.compat.pcm` are never cached, bundled, or published,
  anywhere** — always compiled locally as an ordinary step of every
  build, every time. Measured on real hardware at ~2s to compile from
  scratch (see `yam-toolchain/docs/targets.md`), so there's no build-time
  incentive to cache it; separately, a `.pcm`'s module interface format is
  not stable across Clang versions *or* differing compile flags, so
  precompiling and distributing it centrally would be fragile in ways a
  toolchain-version-and-triple-level cache key can't fully capture.
  Building it fresh for the exact invocation that needs it sidesteps that
  class of bug entirely. `libc++.a`/`libc++abi.a`/`libunwind.a` are a
  genuinely different case (~20-30s, ordinary ABI stability) and *are*
  worth caching/fetching — that's what the target artifact contains
  instead.
* **Target resolution (v1)**: no explicit `yam target add` command. When a
  build references a target triplet whose `libcxx/` (`libc++.a`/
  `libc++abi.a`/`libunwind.a`) isn't already present (bundled or previously
  built), `yam` builds it locally on first use via the currently-pinned
  toolchain (~20-30s — see `yam-toolchain/docs/targets.md`) and caches it
  under `$HOME/.yam/targets/<triple>/` for subsequent builds, transparently
  as part of `yam build`. Fetching a precompiled target artifact from
  `yam-toolchain`'s published OCI artifacts instead of building locally is
  a possible later optimization (it would slot into the same implicit
  resolution flow, not add a new command) — not required for v1, since the
  local-build cost is already small. `std.pcm`/`std.compat.pcm` are not
  part of this resolution or this cache at all — they compile fresh as an
  ordinary step of every build, every time, independent of where `libcxx/`
  came from.
* **`yam toolchain` command surface**: resolved — `yam` gets
  `yam toolchain install <version>` / `yam toolchain update`, an explicit
  command surface (unlike target resolution above) since switching the
  active compiler version is a deliberate action, not something to trigger
  implicitly off a build. See [release.md](release.md)'s Two-Tier Path
  Resolution Fallback Rule, which now also covers toolchain resolution.
* **C++ module interface extraction**: resolved — developers write module
  logic in a single file (no manual `.cppm`/`.cpp` split); `yam` runs
  `yam-iface` once at `yam publish` time to mechanically derive a thin,
  declarations-only public interface plus a compiled `.a`/`.so` for
  everything ordinary. `yam-iface` is built by `yam-toolchain` and resolved
  from the current project's pinned toolchain, same as every other
  Clang-derived tool — never embedded in `yam` itself. See
  [modules.md](modules.md) for the full design: extraction rules, why the
  tool is toolchain-pinned, when it runs, and the `yam iface` preview
  command.
* **CI runner requirements**: resolved to "probably hosted runners
  suffice," pending real measurement once `yam-toolchain`'s single-stage
  pipeline exists — see `yam-toolchain/docs/ci.md`.

## Open Questions (not yet decided)

* **Versioning/tagging scheme**: how `yam-toolchain` releases (both
  toolchain and target artifacts) are tagged on GHCR, and how a `yam`
  release pins which toolchain/target versions it bundles by default.
* **Bootstrap problem**: how a fresh `yam` install obtains its *first*
  toolchain artifact before `yam` itself is present to pull it (likely an
  installer script, similar to `rustup`, rather than solved inside `yam`).
* **Default bundled skeleton set**: exactly which targets' lightweight
  skeletons (not the full `libcxx/` bundle) get vendored into every `yam`
  install by default, beyond the host's own native triplet.
