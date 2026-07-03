use anyhow::Result;

use super::hooksweep;

pub fn run(branch: &str) -> Result<()> {
    let r = hooksweep::sweep(branch, "down", "ws-down")?;

    if !r.failed.is_empty() {
        return Err(super::op_failure(format!(
            "down hook failed for: {}",
            r.failed.join(", ")
        )));
    }

    let mut lines = Vec::new();
    if !r.ran.is_empty() {
        lines.push(format!("Down: {}", r.ran.join(", ")));
    }
    if !r.skipped.is_empty() {
        lines.push(format!("No down hook: {}", r.skipped.join(", ")));
    }
    crate::ui::summary_ok(&lines);
    crate::ui::nudge(&format!("ws up {branch} to resume"));
    Ok(())
}
