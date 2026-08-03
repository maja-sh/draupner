//! Cooperative Ctrl-C handling.
//!
//! SIGINT reaches the whole foreground process group, so a running hook or
//! `git` child already dies on its own — `ws` deliberately does not kill
//! anything. It only flips a flag that looping ops poll between repos
//! (`ops::bail_if_interrupted`), so a run stops at a repo boundary.

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Best-effort: if a handler is already installed (an embedding process, say),
/// default SIGINT behavior stands rather than `ws` refusing to start.
pub fn install() {
    let _ = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst));
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}
