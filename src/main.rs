// mimalloc on every target. On Linux (#27, 1k-unit no-op builds) it beats glibc's
// allocator by 17-19% and musl's, in the static release builds, by 24-38%. macOS and
// Windows haven't been measured yet.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    yet_another_make::run()
}
