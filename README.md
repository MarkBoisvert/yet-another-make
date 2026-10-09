# Yet Another Make (YAM)

A cargo-style package manager and build tool for modern C++.

YAM aims to do for C++ what Cargo did for Rust: one tool, one manifest, one
command to fetch dependencies, build, and run — with first-class support for
C++20 modules and full support for legacy header-based code. It's meant to
replace the usual stack of Conan + CMake + Make/Ninja with a single binary
and a single config file.

> **Status: early development.** YAM is a work in progress and not yet
> usable for real projects. The design goals below describe where the
> project is headed; check back for progress.

## Why

Building C++ today usually means stitching together a package manager
(Conan, vcpkg), a build system generator (CMake), and an actual build tool
(Make, Ninja) — each with its own config language, mental model, and failure
modes. C++ modules make this worse, since most of that tooling predates
modules and only supports them awkwardly.

YAM's goal is to collapse that stack:

- **One manifest** describes your package, its dependencies, and its build
  targets — no separate `CMakeLists.txt` and `conanfile.py`.
- **One command** builds your project, resolving and building dependencies
  as needed.
- **First-class modules** — YAM understands C++ module dependencies
  natively instead of bolting them onto a header-oriented model.
- **Legacy-friendly** — traditional headers and `#include`-based code are
  fully supported, so you don't need modules to use YAM, and you can mix
  both in the same project.

## Planned features

- Package management: publish, fetch, and lock dependencies, similar to
  `cargo add` / `Cargo.lock`.
- Incremental, parallel builds driven by a dependency graph derived
  directly from your source (including module interfaces).
- A single `Yam.toml`-style manifest per package.
- Reproducible builds across compilers and platforms.

None of the above is implemented yet — the repository currently contains
only the initial project skeleton.

## Installing

```sh
cargo install yet-another-make
```

This installs the `yam` binary. Only `yam init` is available so far; see the
[roadmap](https://github.com/MarkBoisvert/yet-another-make/blob/main/docs/roadmap.md)
for what's next.

## Building from source

YAM is written in Rust.

```sh
git clone https://github.com/MarkBoisvert/yet-another-make.git
cd yet-another-make
cargo build --release
```

The resulting binary is `target/release/yam`.

## Contributing

This project is very early — expect the design and the code to change
significantly. Issues and discussion are welcome.

## License

YAM is dual-licensed under either of

- [MIT license](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option, following the same convention as Cargo and the rest of the
Rust ecosystem. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this project by you shall be
dual-licensed as above, without any additional terms or conditions.
