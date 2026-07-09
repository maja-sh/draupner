pub mod init;
pub mod claude;
pub mod new;
pub mod hooksweep;
pub mod teardown;
pub mod up;
pub mod down;
pub mod rm;
pub mod list;
pub mod here;
pub mod clean;
pub mod exec;
pub mod setup;
pub mod remote_check;

#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub msg: String,
    /// When true, `main` exits with `code` but prints no failure banner — the
    /// command being wrapped already reported the failure (see `passthrough_exit`).
    pub silent: bool,
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ExitError {}

pub(crate) fn preflight(msg: impl Into<String>) -> anyhow::Error {
    ExitError { code: 2, msg: msg.into(), silent: false }.into()
}

pub(crate) fn op_failure(msg: impl Into<String>) -> anyhow::Error {
    ExitError { code: 1, msg: msg.into(), silent: false }.into()
}

/// Run a repo's declared lifecycle `cmd` for `event` on the remote server: flush
/// the worktree's mutagen sync so the server sees the latest files, dispatch the
/// command over SSH in the synced worktree, and map the outcome to a `Result`.
/// Shared by up/down (hooksweep), post-create (new), and down/post-remove
/// (teardown) so the flush → dispatch → status-mapping spine lives in one place;
/// callers keep only their own skip decision, step UI, and bookkeeping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_remote_event(
    r: &crate::config::RemoteConfig,
    root: &std::path::Path,
    branch: &str,
    sanitized: &str,
    repo: &str,
    event: &str,
    cmd: &str,
    extra_env: &[(String, String)],
) -> anyhow::Result<()> {
    // Best-effort: the session may not exist yet, or mutagen may be absent (see
    // `ws exec`) — a failed flush shouldn't block the dispatch.
    let _ = crate::mutagen::flush(&crate::mutagen::session_name(branch, repo));
    let base = r.worktree_base(root);
    match crate::remote::run_event(&r.connection, &base, sanitized, repo, event, cmd, extra_env) {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(op_failure(format!("{event} script exited {}", s.code().unwrap_or(-1)))),
        Err(e) => Err(op_failure(format!("{event} on {}: ssh error: {e}", r.connection))),
    }
}

/// Propagate a child process's exit code without printing a failure banner.
/// `ws exec` is a transparent runner — when the command it ran exits non-zero,
/// the command has already reported why, so `ws` mirrors the code and stays
/// silent (`silent: true`; `main` skips the banner).
pub(crate) fn passthrough_exit(code: i32) -> anyhow::Error {
    ExitError { code, msg: String::new(), silent: true }.into()
}

/// Return the interrupt error (exit 130) if a Ctrl-C has been observed. Called
/// by looping ops at each repo boundary so an interrupt stops the sweep cleanly
/// instead of proceeding into the next repo. Convention: 130 = SIGINT.
pub(crate) fn bail_if_interrupted() -> anyhow::Result<()> {
    if crate::signal::interrupted() {
        return Err(ExitError { code: 130, msg: "interrupted".into(), silent: false }.into());
    }
    Ok(())
}
