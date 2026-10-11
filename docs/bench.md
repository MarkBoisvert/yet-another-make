# Benchmarks

yam's performance gates (#28, see [build.md](build.md#performance-gates)) are measured
with the `bench` workspace member. This page records the results, starting with the
CMake + ninja baseline that yam has to beat.

## Running

```sh
cargo run -p bench --release -- run      # every scenario × style × size; hours
cargo run -p bench --release -- report   # print the tables below
```

- **Projects:** `bench run` generates its projects in `target/bench/` (`bench gen`,
  #18), configures CMake + ninja (`bench configure`), and builds each project once so
  every scenario starts up to date.
- **Timing:** it times each scenario with `hyperfine`, using `--prepare` to apply the
  scenario's edit (`bench edit`) or, for `cold`, to clean before each run.
- **Saving and resuming:** results are saved to `target/bench/results.json` after
  every measurement. Cells already there are skipped, so a run can be stopped and
  resumed.
- **Narrowing a run:** `--styles`, `--sizes`, `--scenarios` and `--jobs` pick a subset.
- **Timing yam:** `--yam <path>` adds a `yam build` column, once yam can build
  multi-file targets (#20).

## Method

- **Same sources for both tools.** Each project is one source tree with both a
  `Yam.toml` and a `CMakeLists.txt`.
- **Two styles over the same seeded graph:**
  - **modules:** `.cppm` + `.cpp`, `import std`, C++26
  - **legacy:** `.hpp` + `.cpp`, C++17
- **Graph shape:** 8 layers above a root unit, with each unit depending on 3 units of
  the layer below.
- **Same build configuration everywhere:**
  - the same `clang++` and libc++
  - Debug flags
  - `-j` set to the number of cores for every tool
- **Which tool each scenario is timed with:**
  - `noop` and `touch` are timed with `ninja` alone, because their gates compare
    against it. `noop` is timed with `cmake --build` as well.
  - Every other scenario is timed with `cmake --build`, which includes CMake's own
    regeneration check and reconfigures when `add-unit` changes `CMakeLists.txt`.
- **Runs:** 5 per cell. The heavy scenarios (`cold`, `touch`, `mid-iface`,
  `root-iface`) get 2 runs at 1k units and above, which keeps the whole grid to a
  few hours.

| Scenario | Change | Expected rebuild |
|---|---|---|
| `cold` | clean build | everything |
| `noop` | nothing | nothing |
| `touch` | root interface mtime only | everything for ninja; nothing for a content-hashing tool |
| `leaf-impl` | one leaf function body | 1 TU + link |
| `leaf-iface` | one leaf interface | the leaf and `main` |
| `impl-1pct` / `impl-10pct` | function bodies in 1% / 10% of units | 1% / 10% of TUs |
| `mid-iface` | interface with the median dependent count | about half |
| `root-iface` | the interface everything depends on | everything |
| `add-unit` | a new unit used by `main` | the new unit and `main` (CMake reconfigures) |

## Baseline: CMake + ninja

Measured on 2026-10-11 with `bench run` defaults. yam's column fills in from #20 on.

**What the baseline shows** (the bars the #28 gates set):
- **No-op builds grow with size.** ninja alone takes 6 ms / 35 ms / 509 ms for
  legacy and 8 ms / 54 ms / 1.16 s for modules at 100 / 1k / 10k units. These are
  the times yam's no-op build must beat.
- **`touch` rebuilds everything.** At 10k units, an mtime-only change to the root
  interface costs ninja 544 s (modules) and 1720 s (legacy), as much as a cold
  build. With content hashing it should cost yam about as much as a no-op build.
- **Small edits have a large fixed cost at scale.** At 10k units, `leaf-impl` (one
  translation unit) takes 13.2 s for modules and 5.0 s for legacy. At 100 units it
  takes 0.24 s and 0.58 s. #27 profiles where yam's own time goes. The per-step
  breakdown for CMake (archiving, linking, module dependency handling) isn't
  measured here.
- **Modules build much faster than headers.** A cold build takes 620 s for
  modules vs 1687 s for legacy at 10k units, and `root-iface` 562 s vs 1746 s.
- **Noise:** heavy cells at 1k+ have only 2 runs, so treat their ± as rough. Most
  cells vary by a few percent between runs on this laptop under WSL2. The worst is
  legacy 10k `leaf-impl`, at ±18%.
- **Raw data:**
  [`bench/results/2026-10-11-i7-1260P.json`](../bench/results/2026-10-11-i7-1260P.json).
  Re-render it with `bench report <file>`.

| | |
|---|---|
| clang++ | /usr/bin/clang++ (22.1.8) |
| cmake | cmake version 4.4.4 |
| cores | 12 |
| cpu | 12th Gen Intel(R) Core(TM) i7-1260P |
| hyperfine | hyperfine 1.18.0 |
| jobs | 12 |
| memory | 11 GiB |
| ninja | 1.13.2 |
| os | Linux 6.18.33.2-microsoft-standard-WSL2 x86_64 |

### legacy — 100 units

| Scenario | ninja | cmake --build | yam build |
|---|---|---|---|
| `cold` | — | 16.31 s ± 205 ms (n=5) | — |
| `noop` | 6.1 ms ± 0.4 ms (n=5) | 11 ms ± 0.5 ms (n=5) | — |
| `touch` | 16.05 s ± 129 ms (n=5) | — | — |
| `leaf-impl` | — | 582 ms ± 7.0 ms (n=5) | — |
| `leaf-iface` | — | 655 ms ± 8.1 ms (n=5) | — |
| `impl-1pct` | — | 570 ms ± 3.9 ms (n=5) | — |
| `impl-10pct` | — | 1.67 s ± 17 ms (n=5) | — |
| `mid-iface` | — | 8.36 s ± 23 ms (n=5) | — |
| `root-iface` | — | 16.09 s ± 120 ms (n=5) | — |
| `add-unit` | — | 689 ms ± 8.7 ms (n=5) | — |

### legacy — 1000 units

| Scenario | ninja | cmake --build | yam build |
|---|---|---|---|
| `cold` | — | 156.67 s ± 6.39 s (n=2) | — |
| `noop` | 35 ms ± 1.6 ms (n=5) | 39 ms ± 1.1 ms (n=5) | — |
| `touch` | 162.01 s ± 9.62 s (n=2) | — | — |
| `leaf-impl` | — | 819 ms ± 4.8 ms (n=5) | — |
| `leaf-iface` | — | 917 ms ± 22 ms (n=5) | — |
| `impl-1pct` | — | 1.90 s ± 22 ms (n=5) | — |
| `impl-10pct` | — | 16.34 s ± 199 ms (n=5) | — |
| `mid-iface` | — | 85.98 s ± 520 ms (n=2) | — |
| `root-iface` | — | 161.83 s ± 1.42 s (n=2) | — |
| `add-unit` | — | 897 ms ± 9.2 ms (n=5) | — |

### legacy — 10000 units

| Scenario | ninja | cmake --build | yam build |
|---|---|---|---|
| `cold` | — | 1687.06 s ± 6.68 s (n=2) | — |
| `noop` | 509 ms ± 25 ms (n=5) | 488 ms ± 18 ms (n=5) | — |
| `touch` | 1720.02 s ± 7.49 s (n=2) | — | — |
| `leaf-impl` | — | 5.04 s ± 923 ms (n=5) | — |
| `leaf-iface` | — | 5.13 s ± 264 ms (n=5) | — |
| `impl-1pct` | — | 21.98 s ± 1.03 s (n=5) | — |
| `impl-10pct` | — | 177.74 s ± 530 ms (n=5) | — |
| `mid-iface` | — | 704.82 s ± 1.96 s (n=2) | — |
| `root-iface` | — | 1745.80 s ± 6.31 s (n=2) | — |
| `add-unit` | — | 5.19 s ± 160 ms (n=5) | — |

### modules — 100 units

| Scenario | ninja | cmake --build | yam build |
|---|---|---|---|
| `cold` | — | 6.91 s ± 314 ms (n=5) | — |
| `noop` | 8.3 ms ± 0.4 ms (n=5) | 13 ms ± 0.4 ms (n=5) | — |
| `touch` | 4.38 s ± 171 ms (n=5) | — | — |
| `leaf-impl` | — | 240 ms ± 2.7 ms (n=5) | — |
| `leaf-iface` | — | 811 ms ± 13 ms (n=5) | — |
| `impl-1pct` | — | 236 ms ± 1.6 ms (n=5) | — |
| `impl-10pct` | — | 471 ms ± 20 ms (n=5) | — |
| `mid-iface` | — | 3.00 s ± 111 ms (n=5) | — |
| `root-iface` | — | 4.94 s ± 89 ms (n=5) | — |
| `add-unit` | — | 838 ms ± 6.0 ms (n=5) | — |

### modules — 1000 units

| Scenario | ninja | cmake --build | yam build |
|---|---|---|---|
| `cold` | — | 55.47 s ± 2.33 s (n=2) | — |
| `noop` | 54 ms ± 1.0 ms (n=5) | 59 ms ± 1.5 ms (n=5) | — |
| `touch` | 42.00 s ± 569 ms (n=2) | — | — |
| `leaf-impl` | — | 754 ms ± 6.6 ms (n=5) | — |
| `leaf-iface` | — | 1.39 s ± 7.8 ms (n=5) | — |
| `impl-1pct` | — | 941 ms ± 8.7 ms (n=5) | — |
| `impl-10pct` | — | 4.35 s ± 375 ms (n=5) | — |
| `mid-iface` | — | 28.23 s ± 1.73 s (n=2) | — |
| `root-iface` | — | 52.19 s ± 1.67 s (n=2) | — |
| `add-unit` | — | 2.17 s ± 135 ms (n=5) | — |

### modules — 10000 units

| Scenario | ninja | cmake --build | yam build |
|---|---|---|---|
| `cold` | — | 619.92 s ± 14.52 s (n=2) | — |
| `noop` | 1.16 s ± 83 ms (n=5) | 1.17 s ± 31 ms (n=5) | — |
| `touch` | 544.01 s ± 26.86 s (n=2) | — | — |
| `leaf-impl` | — | 13.18 s ± 203 ms (n=5) | — |
| `leaf-iface` | — | 16.30 s ± 207 ms (n=5) | — |
| `impl-1pct` | — | 15.05 s ± 1.06 s (n=5) | — |
| `impl-10pct` | — | 53.39 s ± 1.50 s (n=5) | — |
| `mid-iface` | — | 285.17 s ± 205 ms (n=2) | — |
| `root-iface` | — | 562.11 s ± 21 ms (n=2) | — |
| `add-unit` | — | 21.12 s ± 224 ms (n=5) | — |
