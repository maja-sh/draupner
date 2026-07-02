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

// NOTE: SIGINT / cooperative-cancellation handling deferred to spec-2. The
// earlier scaffold installed a `ctrlc` handler that set a flag no code ever
// polled, so Ctrl-C ended up swallowed. Wire it up properly (poll between
// hook invocations, propagate SIGINT to child processes) when the ops that
// need it land.
