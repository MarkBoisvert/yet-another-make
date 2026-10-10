# Package Management & Registry Distribution

## Overview

In these docs a **package** is the published artifact that `yam publish` produces
from a **project** (the source tree with a `Yam.toml`); see
[glossary.md](glossary.md).

`yam` is a package manager in the spirit of Cargo (manifest-driven dependency
declarations, lockfile-based reproducible resolution, `yam add`/`yam build`
workflow), but it diverges from Cargo in how packages are distributed:
instead of a central index + blob store (crates.io-style), `yam` packages are
published to and fetched from **OCI-compliant container registries**, with
**GitHub Container Registry (ghcr.io)** as the primary/default registry.

Packages are pushed and pulled as OCI artifacts rather than as a bespoke
archive format, which means standard OCI tooling (`docker`, `oras`, registry
UIs, GHCR's built-in package permissions/visibility model) can inspect and
manage `yam` packages without `yam`-specific tooling.

## Relationship to toolchain/target artifacts

This same OCI-artifact-via-GHCR model is also how `yam-toolchain` publishes
host toolchain builds and target-triplet artifacts (sysroots, compiled
`libc++` bundles — never `std.pcm`/`std.compat.pcm`, which are always
built locally instead) — see [toolchains.md](toolchains.md). The
intent is one shared artifact-fetch code path in `yam` (pull, verify
digest, unpack) reused across user packages, toolchain versions, and
target artifacts, so the OCI schema decisions below (media type,
naming/tagging, digest pinning) should stay consistent with whatever
`yam-toolchain` needs rather than being designed twice.

## Why OCI / ghcr instead of a custom index

* Reuses existing, battle-tested registry infrastructure (auth, storage,
  CDN, garbage collection, visibility/permissions) instead of `yam` having
  to run and operate its own index + blob storage service.
* GitHub repos already commonly host the project source *and* can host its
  published artifacts (via GHCR) under the same namespace/auth model,
  simplifying publishing workflows for open-source `yam` packages.
* OCI artifacts are content-addressed and immutable by digest, which maps
  naturally onto lockfile-based reproducible builds.

## Open Questions (not yet decided)

The following need to be designed before `yam add` / `yam publish` can be
implemented. Capturing them here so design discussion has a fixed home.

* **Artifact layout**: partially resolved for module-source libraries — see
  [modules.md](modules.md)'s "Package artifact shape for module-source
  libraries": an extracted, declarations-only `.cppm` interface (source,
  compiled locally by each consumer) plus a compiled `.a`/`.so`, never a
  precompiled `.pcm`, for the same BMI-instability reasons `std.pcm` is
  never shipped (see [toolchains.md](toolchains.md)). Still open for other
  package kinds: header-only packages, packages with no exported module
  interface at all, and whether/how prebuilt binaries are bundled per
  target triplet within one artifact vs. a multi-arch (OCI image index)
  artifact.
* **Manifest/media type**: custom OCI artifact `mediaType` for `yam`
  packages (e.g. `application/vnd.yam.package.v1+json` manifest with
  layers), so registries and tooling can identify `yam` artifacts.
* **Naming & tagging scheme**: how a `Yam.toml` dependency name +
  version maps to an OCI repository path and tag on `ghcr.io` (e.g.
  `ghcr.io/<owner>/<package>:<version>`), and how this generalizes to
  registries other than GHCR.
* **Versioning**: SemVer requirement, how version ranges in `Yam.toml`
  resolve to OCI tags/digests, and whether resolution pins to immutable
  digests in the lockfile (likely yes, for reproducibility).
* **Authentication**: how `yam` obtains registry credentials (reuse
  `docker`/`~/.docker/config.json` credential helpers? `GITHUB_TOKEN`?
  a `yam login` command?).
* **Local cache layout**: where downloaded package artifacts are cached
  on disk (likely alongside the target-triplet layout described in
  [release.md](release.md), e.g. under `$HOME/.yam/`), and how cache
  invalidation/garbage collection works.
* **Dependency resolution algorithm**: SAT-style resolver vs. simpler
  Cargo-style resolver; how version conflicts across the dependency graph
  are handled.
* **Lockfile format**: `Yam.lock` schema — package name, resolved version,
  resolved digest, registry source, transitive dependency graph.
* **Publishing workflow**: what `yam publish` does end-to-end (build →
  package → push OCI artifact → tag), and whether it integrates with CI
  (e.g. a GitHub Action).
* **Registries other than ghcr**: whether/how `yam` supports alternate or
  self-hosted OCI registries (Docker Hub, self-hosted Zot/Harbor, etc.) via
  registry configuration in `Yam.toml` or a global config file.
