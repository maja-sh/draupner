use anyhow::Result;

use super::hooksweep;

pub fn run(branch: &str) -> Result<()> {
    let r = hooksweep::sweep(branch, "down", "drup-down")?;

    if !r.failed.is_empty() {
        let mut msg = format!("down hook failed for: {}", r.failed.join(", "));
        if !r.needs_human.is_empty() {
            msg.push_str(&format!("; needs human for: {}", r.needs_human.join(", ")));
        }
        return Err(super::op_failure(msg));
    }
    if !r.needs_human.is_empty() {
        return Err(super::interactive_required(format!(
            "down needs an interactive terminal for: {}; re-run `drup down {branch}` from your shell",
            r.needs_human.join(", ")
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
    crate::ui::nudge(&format!("drup up {branch} to resume"));
    Ok(())
}
