use std::sync::atomic::{AtomicBool, Ordering};
use tracing_subscriber::EnvFilter;

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .try_init();
}

pub fn install_signal_handler() {
    let _ = ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
    });
}

#[allow(dead_code)]
pub fn check_interrupted() -> anyhow::Result<()> {
    if INTERRUPTED.load(Ordering::SeqCst) {
        anyhow::bail!("interrupted");
    }
    Ok(())
}
