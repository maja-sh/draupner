use anyhow::Result;

use super::hooksweep;

pub fn run(branch: &str) -> Result<()> {
    let r = hooksweep::sweep(branch, "up", "wsx-up")?;

    if !r.failed.is_empty() {
        let mut msg = format!("up hook failed for: {}", r.failed.join(", "));
        if !r.needs_human.is_empty() {
            msg.push_str(&format!("; needs human for: {}", r.needs_human.join(", ")));
        }
        return Err(super::op_failure(msg));
    }
    if !r.needs_human.is_empty() {
        return Err(super::interactive_required(format!(
            "up needs an interactive terminal for: {}; re-run `wsx up {branch}` from your shell",
            r.needs_human.join(", ")
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
