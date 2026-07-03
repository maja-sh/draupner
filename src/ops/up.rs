use anyhow::Result;

use super::hooksweep;

pub fn run(branch: &str) -> Result<()> {
    let r = hooksweep::sweep(branch, "up", "ws-up")?;

    if !r.failed.is_empty() {
        return Err(super::op_failure(format!(
            "up hook failed for: {}",
            r.failed.join(", ")
        )));
    }

    let mut lines = Vec::new();
    if !r.ran.is_empty() {
        lines.push(format!("Up: {}", r.ran.join(", ")));
    }
    if !r.skipped.is_empty() {
        lines.push(format!("No up hook: {}", r.skipped.join(", ")));
    }
    crate::ui::summary_ok(&lines);
    Ok(())
}
