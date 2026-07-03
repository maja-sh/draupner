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
pub mod use_;
pub mod clean;
pub mod test;

pub(crate) fn not_implemented(name: &str) -> anyhow::Result<()> {
    anyhow::bail!("{name}: not yet implemented (spec-1 scaffold only)")
}

#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub msg: String,
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ExitError {}

pub(crate) fn preflight(msg: impl Into<String>) -> anyhow::Error {
    ExitError { code: 2, msg: msg.into() }.into()
}

#[allow(dead_code)]
pub(crate) fn op_failure(msg: impl Into<String>) -> anyhow::Error {
    ExitError { code: 1, msg: msg.into() }.into()
}
