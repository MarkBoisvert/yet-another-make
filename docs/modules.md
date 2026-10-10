# C++ Module Interface Extraction

## Overview

`yam` lets developers write C++ module logic in a single file, without
manually splitting a `.cppm` interface from a `.cpp` implementation the way
traditional module authoring style expects. At publish time, `yam` runs a
dedicated tool, `yam-iface`, that mechanically derives two artifacts from
that single file: a thin, declarations-only public interface (for consumers
to compile against their own toolchain) and the ordinary function/method
bodies, compiled once into the library's `.a`/`.so`. Developers get the
convenience of one file; consumers still get a real compiled binary to link
against instead of recompiling the library's logic themselves.

## Why this is needed at all

Two simpler-looking alternatives were considered and rejected:

* **Ship the full, unsplit source as the "interface."** Standard
  module-dependency tooling (clang-scan-deps/P1689-driven builds, CMake's
  module support) treats any module interface unit as needing both a
  BMI-producing step *and* an object-compiling step. If the shipped source
  still contains ordinary function bodies, the consumer's own build compiles
  those bodies into a second object file that collides with the symbols
  already present in the shipped `.a`/`.so` — duplicate-symbol errors at
  best, silent ODR duplication at worst. There's no reliable way to make
  arbitrary consumer tooling skip that second phase for one specific import.
* **Ship the precompiled `.pcm` directly, matched against the consumer's
  flags, falling back to a local compile on mismatch.** This is the same
  idea already rejected for `std.pcm` (see `toolchains.md`), for the same
  reason: a `.pcm`'s BMI format isn't stable across Clang versions or
  compile flags, so a cache keyed at anything less than exact-invocation
  granularity degrades over time as `yam toolchain update` moves the
  ecosystem's Clang version forward — published packages effectively stop
  matching shortly after publish, and the fallback path still needs a
  from-source compile that hits the exact duplicate-symbol problem above.
  It doesn't remove the need for extraction, it just makes it conditional
  while adding a second, low-hit-rate code path.

Extracting a thin, declarations-only interface at publish time avoids both
problems structurally: there's no body left for a naive consumer build to
recompile, and nothing BMI-shaped is ever cached or shipped.

## What gets extracted

Only top-level declarations marked `export` are considered at all —
everything else in the source is implementation detail regardless of how
it's written.

* **Free functions, variables, type aliases, namespaces, concepts**:
  included as declarations if `export`ed. Bodies are stripped unless the
  declaration is a template, `constexpr`/`consteval`, or otherwise required
  to stay inline (see below) — ordinary bodies are compiled separately into
  the library's `.a`/`.so`.
* **Classes/structs**: if `export`ed, the *entire* member list is included
  regardless of access specifier — every data member, every method
  declaration, nested types, friend declarations, base classes, default
  member initializers, static asserts. Filtering by visibility (e.g.
  public/protected only) was considered and rejected:
  * Private *and* protected data members still determine `sizeof`/layout
    for any type used by value anywhere in the exported surface (a
    parameter, return type, member of another exported class, container
    element) — omitting them produces a class whose layout the consumer's
    compiler computes differently than the actual compiled implementation,
    which is memory corruption, not information hiding.
  * `protected` is laid out identically to `private` — it offers no
    layout-avoidance benefit over keeping it, since the goal (if any) was
    hiding implementation details, and protected is exactly as much of an
    implementation detail as private for this purpose.
  * C++ modules have no per-member `export` granularity to begin with — a
    class is one indivisible declaration as far as the module interface is
    concerned; extraction can select whole declarations, not fields within
    one.
* **Method/function bodies**: stripped for anything ordinary — non-template,
  non-`constexpr`/`consteval`, not implicitly inline by virtue of being
  defined directly inside the class body. Those bodies get compiled once,
  separately, into the shipped `.a`/`.so`.
* **Bodies that must stay in the interface**: templates (the consumer's
  compiler has to instantiate them locally — no way around this), `constexpr`/
  `consteval`, and anything implicitly inline (defined in-class). This is an
  unavoidable C++ constraint, identical whether a human hand-splits the file
  or a tool does it automatically — it's the same reason libc++'s own
  `std.cppm` still contains full bodies for template-heavy parts of the
  standard library while re-exporting ordinary functions (`std::terminate`,
  iostream operations, etc.) as declarations only, backed by `libc++.a`. See
  "Measured build costs" in `../../yam-toolchain/docs/targets.md` for why
  that split already works at the scale of the standard library itself.

## `yam-iface`: a separate, toolchain-pinned tool, not embedded in `yam`

`yam-iface` is built with Clang's LibTooling/AST libraries — the same
infrastructure `clang-tidy` and `clang-doc` are built on — and walks the AST
Clang's own frontend already constructed, applying the keep/strip rules
above using facts (is this a template, is this implicitly inline, is this a
class member) the compiler has already computed. It is not a hand-rolled
C++ parser.

It is built and published by `yam-toolchain` alongside `clang`,
`clang-tidy`, `clang-format`, `clang-scan-deps`, and `clangd` (see
`../../yam-toolchain/docs/architecture.md`), and invoked externally by
`yam`, resolved from whichever toolchain version the current project is
pinned to — **not** embedded in the `yam` binary itself, consistent with the
project's standing decision not to link LLVM into `yam`. Two reasons this
applies to `yam-iface` specifically, not just "the existing rule":

* **Correctness requires matching the exact Clang version compiling the
  code.** The extraction rules above depend on getting version-specific
  semantic nuances right (implicit-inline rules, defect-report resolutions,
  whatever new language features a given Clang version supports). A tool
  frozen at whatever LLVM snapshot `yam` itself was built with either can't
  parse newer syntax at all, or silently makes wrong keep/strip decisions
  for edge cases whose interpretation shifted between versions.
* **LibTooling/Clang's AST API is not a stable interface across LLVM
  releases** — a tool built against it has to be compiled against a specific
  LLVM version's headers to link at all, which pins it to one release by
  construction, independent of the language-evolution argument above.

That the extracted `.cppm` *output* is portable, toolchain-agnostic text
doesn't change this — `clang-format` is the existing precedent for exactly
this distinction: its output (reformatted source) is equally portable, yet
it's still shipped and version-pinned per-toolchain, because its parsing
behavior genuinely shifts between Clang releases. See
`../../yam-toolchain/CLAUDE.md` and `../../yam-toolchain/docs/architecture.md`.

Embedding `yam-iface` in `yam` itself was rejected for the same reasons any
other LLVM-linked capability was: it would freeze extraction at one LLVM
snapshot (breaking per-project toolchain pinning via `yam toolchain
install`/`update`), decouple it from `yam-toolchain`'s own release cadence,
and bloat the `yam` binary for no measurable speed benefit — subprocess
spawn cost is negligible next to actual compile times (milliseconds against
the multi-second/tens-of-seconds costs already measured for `std.pcm` and
`libcxx/` in `../../yam-toolchain/docs/targets.md`).

## When extraction runs

* **Not on every ordinary `yam build`.** Local dev iteration on the
  library's own code just compiles the full, unsplit source directly — no
  `.a`/`.so` has been shipped yet for a stripped-down copy to conflict with,
  so there's nothing for extraction to protect against.
* **At `yam publish` (packaging time).** This is the only point extraction's
  output is actually consumed — the published package artifact. Publishing
  already requires a full release-mode compile to produce the `.a`/`.so`
  anyway, so running `yam-iface` once at that same moment is sufficient. One
  extraction per published version, not per build.
* **Recommended during CI / `yam build --release` validation**, ahead of the
  actual publish gate, so a template/inline corner case surfaces as an
  ordinary CI failure during iteration rather than a surprise at release
  time. Not required, but cheap insurance.
* **Never re-run on the consumer side.** A consumer receives the
  already-extracted `.cppm` as part of the package and compiles it into
  their own local `.pcm` on first use, cached under `$HOME/.yam/...` —
  exactly the same "build locally on first use, cache the result" shape
  already decided for target-triplet resolution (see `toolchains.md`'s
  "Target resolution (v1)"). That's an ordinary `--precompile` of
  already-thin source, not extraction — extraction only ever happens once,
  upstream, at the author's publish time.

## `yam iface`: interface preview command

`yam-iface` follows the `yam-<verb>` PATH-dispatch convention — the same
mechanism Cargo uses for `cargo-<verb>` plugins (`cargo-watch`,
`cargo-expand`, etc.), originally from Git's `git-<verb>` dispatch. If
`yam-iface` is resolvable from the current toolchain, `yam iface` invokes it
directly, letting a library author preview exactly what interface will ship
*before* publishing — catching an accidental export, or checking how a
tricky template/inline case got handled — rather than finding out only at
`yam publish` time.

Named `yam-iface` rather than the more literal `yam-extract` specifically to
avoid colliding with a plausible future, unrelated `yam extract` (e.g.
extracting a downloaded package archive).

## Package artifact shape for module-source libraries

A published module-source library package contains:

* the extracted, declarations-only `.cppm` interface — portable source,
  recompiled locally into a `.pcm` by every consumer on first use, never
  shipped as a precompiled binary;
* the compiled `.a`/`.so` — containing the bodies of everything ordinary
  (non-template, non-inline, non-`constexpr`) that extraction stripped out.

Never a precompiled `.pcm`, for the same reasons `std.pcm` is never shipped
— see `toolchains.md`. This resolves, for module-source libraries
specifically, the "Artifact layout" open question in `packages.md`; other
package kinds (header-only, prebuilt-binary-only) remain open there.
