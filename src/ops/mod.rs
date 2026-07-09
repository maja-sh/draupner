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
