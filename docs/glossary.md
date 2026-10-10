# Glossary

The terms the docs, issues and code use. Where they differ from Cargo, that's
deliberate: in yam the source you build and the artifact you publish are different
things.

| Term | Meaning |
|---|---|
| **Project** | One directory with one `Yam.toml`, identified by the `[project]` table (`name`, `version`, `std`). It's what you build. |
| **Target** | What a project builds: its library (at most one) and any number of binaries. Found by convention or declared with `[lib]` and `[[bin]]`. |
| **Package** | The published, versioned artifact that `yam publish` produces from a project: a declarations-only interface (extracted by `yam-iface`) plus prebuilt `.a`/`.so` per target triplet, distributed as an OCI artifact (see [packages.md](packages.md)). It's what others depend on. |
| **Dependency** | An entry in `[dependencies]`. It resolves either to a **package** from a registry, or to a local **project** (via `path` or `git`) that's built from source. |
| **Root project** | The project `yam build` runs in. |
| **Build graph** | What one `yam build` compiles: the root project plus all of its dependencies, for one profile and target triplet. |
| **Workspace** (planned) | Several projects under a shared top-level `Yam.toml`, built together. |
| **Profile** | `Debug` or `Release`; each has its own directory, `target/<Profile>/`. |
| **Project standard** | The single C++ standard all of a project's own files compile at: `max(own std, declared std of each direct dependency)`. See [build.md](build.md). |

The crate on crates.io is named `yet-another-make`. That's Cargo's sense of
"package" and is unrelated to yam packages.
