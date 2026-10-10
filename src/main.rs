// Instructs Rust to use mimalloc instead of musl's default allocator
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    yet_another_make::run()
}
