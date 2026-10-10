# Release & Distribution Layout

## Target Distribution Channels

* **Linux**: Native System Packages (RPM, DEB), Canonical Snap, and Local User Space ($HOME/.yam/).
* **Windows**: Flat Application Directory (C:\Program Files\YAM\ or %LOCALAPPDATA%\YAM\).
* **macOS**: Apple Disk Image (.dmg) and Homebrew archive payload.

## Target Layout Structures by Operating System

### 1. Linux & macOS (Namespaced FHS Hierarchy)

The destination directory anchor (root) changes dynamically based on your chosen distribution channel: 

* **DEB / RPM Packages**: root maps directly to /usr (System package manager standard).
* **Manual Admin Install**: root maps to /usr/local.
* **Isolated User Install**: root maps explicitly to $HOME/.yam/.

#### Linux: System/Core Layout Tree (Read-Only / Pre-Bundled)

This directory tree houses the host toolchain executables and a **curated
set** of pre-packaged target environments that ship with the core
application installer — not just one. To satisfy the strict guidelines of
the Linux Filesystem Hierarchy Standard (FHS), target triplets sit flatly
and directly inside the namespaced lib/yam/ folder, completely omitting any
redundant targets/ middle layer.

Only the **host's own native triplet** gets the full bundle, including the
compiled `libcxx/lib/` archives (`libc++.a`, `libc++abi.a`, `libunwind.a`)
— that's the expensive part (see [toolchains.md](toolchains.md)'s
"Resolved decisions" for measured build costs). The rest of the curated
set (exact list: see toolchains.md's "Default bundled skeleton set" open
question) ships as **lightweight skeleton only** — headers, `crt/`, and
ABI-stub link data, cheap enough to bundle broadly. Compiling a program
against one of those triplets works immediately for the compile step; the
`libcxx/lib/` archives get built locally, transparently, the first time
they're actually needed (~20-30s — see `yam-toolchain/docs/targets.md`),
with no explicit "add" command in v1 — see toolchains.md's "Resolved
decisions."

**No triplet ever ships with precompiled `std.pcm`/`std.compat.pcm`,
including the host's own.** Those compile locally as an ordinary step of
every build, every time (~2s, negligible — see
`yam-toolchain/docs/targets.md`'s "Measured build costs" and "Implication"
for why caching them isn't just unnecessary but actively counterproductive
given how flag-sensitive a `.pcm`'s module interface format is).

```text
root/
├── bin/                                # Global User-Facing Entry Points
│   └── yam                             # Main orchestrator toolchain binary
│
├── libexec/                            # Private Host Toolchain Components (default; see Toolchain User Extension Layout Tree for overrides)
│   └── yam/                            # Native executables running on host CPU
│       ├── clang                       # Core LLVM compiler driver
│       ├── clang++                     # C++ compilation driver
│       ├── lld                         # High-performance LLVM linker
│       ├── lldb                        # Interactive CLI debugger
│       ├── lldb-server                 # Kernel-level debugging server stub
│       ├── clangd                      # Language Server Protocol (LSP) daemon
│       ├── clang-format                # Formatting framework engine
│       ├── clang-tidy                  # Static analyzer / linter
│       ├── clang-scan-deps             # C++ module dependency scanner (P1689)
│       └── lib/
│           └── clang/
│               └── 22/                 # Tightly coupled Clang version folder
│                   └── include/        # Essential internal resource headers (e.g., stdarg.h)
│
└── lib/                                # Core Architectural Environments (Read-Only Concept)
    └── yam/                            # Namespaced system library repository
        ├── x86_64-linux-glibc2.17/     # Host's own triplet — full bundle
        │   ├── sysroot/                # Isolated target operating system layout root
        │   │   ├── lib/                # Target C runtime libraries (e.g., libc.so.6) and crt*.o
        │   │   └── usr/
        │   │       ├── include/        # Target POSIX C headers (e.g., stdio.h)
        │   │       └── lib/            # Target library link stubs (e.g., libc.so)
        │   ├── crt/                    # LLVM runtime companion components (crtbegin/end)
        │   ├── libgcc/                 # Low-level GCC runtime compatibility layer
        │   └── libcxx/                 # Self-contained C++ standard library payload
        │       ├── include/c++/v1/     # Modern LLVM libc++ headers
        │       └── lib/                # Static compilation archives (libc++.a, libc++abi.a, libunwind.a)
        │           # No modules/ dir here — std.pcm/std.compat.pcm are
        │           # never bundled; always compiled locally, fresh,
        │           # every build (see toolchains.md)
        ├── aarch64-linux-glibc2.31/    # Curated additional triplet — skeleton only
        │   ├── sysroot/
        │   ├── crt/
        │   └── libgcc/                 # No libcxx/ yet — built locally on first use
        └── x86_64-windows-mingw_ucrt/  # (example) skeleton only, same shape
            ├── sysroot/
            ├── crt/
            └── libgcc/
```

#### Linux: User Extension Layout Tree (Write-Space / Dynamically Added)

There is no explicit `yam target add` command in v1. When a build references a target triplet whose `libcxx/` bundle isn't already present — either because the triplet has skeleton-only bundling in the core install, or because it isn't bundled at all — the orchestrator transparently compiles it locally using the currently-pinned toolchain and writes the result into a user-owned sandbox directory within the user's home folder, bypassing the read-only system installation paths entirely. This guarantees that unprivileged, on-the-fly target builds never pollute XDG system targets. 

```text
$HOME/
└── .yam/
    ├── targets/                        # Writable User Extension Target Architecture Silos
    │   └── aarch64-linux-glibc2.31/    # Built locally on first use, cached here
    │       ├── sysroot/
    │       ├── crt/
    │       ├── libgcc/
    │       └── libcxx/                 # lib/ only — no modules/, see note below
    └── toolchain/                      # Writable User Extension Toolchain Versions
        └── 23.0.0/                     # Dynamically installed via 'yam toolchain install <version>'
            ├── clang, clang++, lld, lldb, lldb-server,
            │   clangd, clang-format, clang-tidy, clang-scan-deps
            └── lib/clang/23/include/
```

`yam toolchain install <version>` / `yam toolchain update` fetches a
`yam-toolchain` OCI artifact for the current host platform (see
[toolchains.md](toolchains.md)) into this directory. Switching the active
toolchain version invalidates any already-cached target `libcxx/lib/`
archives (`libc++.a`/`libc++abi.a`/`libunwind.a`) built against the old
version, since they're compiled from that version's libc++ source —
`yam toolchain update` must re-resolve/re-fetch (or locally rebuild)
matching target artifacts for the new toolchain version rather than
leaving a stale `libc++.a` in place. `std.pcm`/`std.compat.pcm` need no
such invalidation step — they're never cached to begin with; each build
recompiles them fresh against whatever toolchain is currently active, so
there's nothing to go stale (see toolchains.md).

#### macOS: Distribution Layout

macOS is not a true FHS platform, so the "root" from the table above is reached differently depending on channel, but both channels converge on the same namespaced tree shown in the Linux System/Core Layout Tree above:

* **Homebrew**: The formula stages files into a version-scoped Cellar directory (`/usr/local/Cellar/yam/<version>/` on Intel, `/opt/homebrew/Cellar/yam/<version>/` on Apple Silicon), and Homebrew symlinks that payload's `bin/`, `libexec/`, and `lib/` into the corresponding Homebrew prefix — equivalent to the Linux **Manual Admin Install** root (`/usr/local`).
* **.dmg**: The disk image mounts a payload with the same `bin/`, `libexec/yam/`, and `lib/yam/` tree, installed by the user to their prefix of choice (defaulting to `/usr/local`) or to `$HOME/.yam/` for an isolated install, matching the Linux **Isolated User Install** case.
* **Code signing & notarization**: Both the `yam` binary and the bundled `clang`/`lld`/`lldb` executables inside the `.dmg` must be signed with a Developer ID and notarized before distribution, or Gatekeeper will block first launch. This is a separate signing operation from `yam-toolchain`'s own signing of its published toolchain artifacts (see `yam-toolchain/docs/ci.md`) — `yam`'s release process re-signs the final bundle it assembles, it doesn't rely on the toolchain artifact's signature propagating through.

User-added targets and toolchain versions on macOS follow the identical `$HOME/.yam/targets/<target-triple>/` and `$HOME/.yam/toolchain/<version>/` layout described in the Linux User Extension Layout Tree above.

### 2. Windows Deployment Structure (Unified Root Layout)

Windows distributions skip the libexec and lib/yam/ hierarchies to conform to native platform conventions and dynamic library linking (.dll) lookup behaviors. All core host tools execute from a single flat folder alongside the main program entry point. Writable target extensions map to local application data storage profiles to avoid corporate network roaming bottlenecks. 

#### Windows: System/Core Layout Tree (Read-Only / Pre-Bundled)

```text
root\ (C:\Program Files\YAM\ or %LOCALAPPDATA%\YAM\)
├── yam.exe                             # Main toolchain orchestrator entry point
├── clang.exe                           # Native compiler backend driver
├── clang++.exe                         # C++ compilation frontend driver
├── lld.exe                             # Linker engine binary
├── lldb.exe                            # Debugger execution interface
├── libclang.dll                        # Shared core tooling compiler dynamic library
├── lib/
│   └── clang/
│       └── 22/
│           └── include/                # Clang internal resource headers
└── targets\                            # Pre-bundled platform-native Target Repository (curated set — see Linux tree note above; only the host's own triplet gets the full libcxx\ bundle)
    └── x86_64-windows-mingw_ucrt\      # Triplet sits flat here (No intermediate lib folders)
        ├── include\                    # Win32 and UCRT C/C++ system headers
        ├── lib\                        # Win32 import libraries (*.a) and CRT objects
        └── share\                      # Configuration metadata and options manifests
```

#### Windows: User Extension Layout Tree (Write-Space / Dynamically Added)

As on Linux/macOS, there is no explicit `yam target add` command in v1 — when a build on a Windows host references a target triplet that isn't already fully bundled, the orchestrator transparently builds it locally and completely bypasses C:\Program Files\ to avoid administrative UAC permission blocks, writing files cleanly into modern local app data silos. This location must be resolved via the %LOCALAPPDATA% environment variable rather than a hardcoded path — using %APPDATA% instead would incorrectly place target extensions in the roaming profile. 

```text
%LOCALAPPDATA%\ (C:\Users\<Username>\AppData\Local\)
└── YAM\                                # Windows-Compliant Storage Anchor
    ├── targets\                        # Dynamic Target Architecture Silos
    │   └── aarch64-linux-glibc2.31\    # Built locally on first use, cached here
    │       ├── sysroot\
    │       ├── libcxx\
    │       └── share\
    └── toolchain\                      # Dynamically installed toolchain versions
        └── 23.0.0\                     # Via 'yam toolchain install <version>'
```

## Packaging & Execution Mechanics

### 1. Two-Tier Path Resolution Fallback Rule

To seamlessly bridge the architectural gaps between pre-bundled core configurations and downloaded add-ons, the yam binary must resolve **both target folder lookups and toolchain lookups** using a precise lookup sequence: 

1. **Check User Space First**: 

  * On *Linux/macOS*: Verify if a target exists at $HOME/.yam/targets/<target-triple>, or a toolchain version at $HOME/.yam/toolchain/<version>.
  * On *Windows*: Verify if a target exists at %LOCALAPPDATA%\YAM\targets\<target-triple>, or a toolchain version at %LOCALAPPDATA%\YAM\toolchain\<version>.
  * If present, this explicitly overrides system paths, allowing for local target and toolchain updates.
2. **Fallback to Relative Execution Location Second**: 

  * On *Linux/macOS*: Compute path relative to yam's active binary folder at ../lib/yam/<target-triple> (targets) or ../libexec/yam (toolchain — the bundled default).
  * On *Windows*: Compute path relative to yam.exe's active binary folder at .\targets\<target-triple> (targets) or the flat root itself (toolchain — the bundled default).
  * This guarantees that the default target and toolchain map instantly out of the box regardless of deployment type.

### 2. Snap Packaging Restrictions

When packaging for Canonical Snap channels, the snap must use **classic confinement** — the toolchain compiles and links to arbitrary user-chosen output paths outside the snap sandbox, which strict confinement's filesystem isolation would break. Paths under lib/ and libexec/ must still enforce strict confinement rules for the snap's own read-only assets. Fallbacks should read dynamically from the $SNAP and $SNAP_USER_DATA environment variables to cleanly map read-only system assets vs. writable cross-compilation target folder targets. 

### 3. Automated Orchestration

Packaging is driven by the GitHub Actions release workflow on a `v*` tag (see the roadmap's #8). That workflow builds the `yam` binaries (static musl on Linux) and produces the release archives. The native packages described above (RPM, DEB, Snap, .dmg and the Homebrew payload, and the Windows flat layout) are assembled from those binaries in the same workflow. The bundled default toolchain and target trees are not built here: they are pulled from the `yam-toolchain` OCI artifacts described in [toolchains.md](toolchains.md).
