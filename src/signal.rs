//! Cooperative Ctrl-C: the handler flips a flag that looping ops poll between
//! repos (`ops::bail_if_interrupted`) so we stop at a boundary instead of
//! aborting mid-work; children in the foreground group already receive SIGINT
//! directly, and `main` maps the resulting error to exit 130.

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install the SIGINT handler; best-effort, a prior handler (e.g. in an embedder) leaves default behavior.
pub fn install() {
    let _ = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst));
}

/// Has an interrupt been observed since startup?
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}
