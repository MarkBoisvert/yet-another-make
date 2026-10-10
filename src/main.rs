// mimalloc on every target: a big win over musl's allocator in the static Linux
// release builds, and expected to help elsewhere too (confirmed by #27's profiling).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    yet_another_make::run()
}
