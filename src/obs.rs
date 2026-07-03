use tracing_subscriber::EnvFilter;

pub fn init_logging() {
    // Never chatty by default. `RUST_LOG=ws=debug ws <op>` for a full trace.
    // Structured, one event per line, sent to stderr — separate from the
    // user-facing ui:: output (which also goes to stderr but is meant for humans).
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .try_init();
}

// SIGINT / cooperative-cancellation handling lives in `signal.rs`: the handler
// flips a flag that looping ops poll at each repo boundary via
// `ops::bail_if_interrupted` (exit 130). SIGINT reaches hook/git children for
// free via the shared process group, so no manual forwarding is needed.
