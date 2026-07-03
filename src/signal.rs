//! Cooperative Ctrl-C handling.
//!
//! We deliberately do *not* let SIGINT kill `ws` outright. The terminal
//! delivers SIGINT to the whole foreground process group, so a running hook or
//! `git` child already receives it and dies on its own. What `ws` adds is a
//! flag: the handler flips it, and looping ops poll it between units of work
//! (`ops::bail_if_interrupted`) so we stop cleanly at a repo boundary instead
//! of charging into the next repo. `main` maps the resulting error to exit 130.

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install the SIGINT handler. Best-effort — a failure to install (e.g. a
/// handler already set in an embedding process) just leaves default behavior.
pub fn install() {
    let _ = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst));
}

/// Has an interrupt been observed since startup?
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}
