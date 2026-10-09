//! Benchmark harness for `yam`: generates equivalent `yam` and `cmake` + `ninja` projects
//! and times them with `hyperfine`. The generator lands in #18 and the runner in #19.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("bench: not implemented yet (generator: #18, runner: #19)");
    ExitCode::FAILURE
}
