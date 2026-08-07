use tracing_subscriber::EnvFilter;

pub fn init_logging() {
    // Default off; opt in with `RUST_LOG=ws=debug`.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .try_init();
}

// Signal handling lives in `signal.rs`; child processes inherit SIGINT via the shared process group.
