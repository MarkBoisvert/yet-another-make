// mimalloc on every target. On Linux (#27, 1k-unit no-op builds) it beats glibc's
// allocator by 17-19% and musl's, in the static release builds, by 24-38%. macOS and
// Windows haven't been measured yet.
//
// Built with mimalloc's `no_thp` feature: on Linux it doesn't ask for transparent huge
// pages. A no-op build touches ~10 MB, and with huge pages that grew to 30-40 MB of
// 2 MiB pages the kernel had to zero, costing about 1 ms. It only drops mimalloc's
// `madvise` call, so the compilers yam starts keep the system's setting.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    yet_another_make::run()
}
