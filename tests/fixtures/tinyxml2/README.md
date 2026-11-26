# tinyxml2 porting fixture

The porting acceptance test (`tests/porting.rs`, roadmap #20) builds this project
with `yam build`. It shows that an existing header-based C++ library and its CLI
build from a `Yam.toml` alone, with no changes to their source files.

- **Upstream:** <https://github.com/leethomason/tinyxml2>, tag `11.0.0`
  (commit `9148bdf719e997d1f474be6bcc7943881046dba1`).
- **Copied unmodified:** `tinyxml2.cpp`, `tinyxml2.h`, `xmltest.cpp` and
  `LICENSE.txt` (zlib license). Only `Yam.toml` and this README were added.
- **The `xmltest` bin** is upstream's test program. Given a file name, it loads that
  file and reports the parse result, which is what the test checks. Its full test
  suite needs upstream's `resources/` directory, which isn't copied here.
