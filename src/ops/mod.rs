pub mod init;
pub mod claude;
pub mod new;
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
