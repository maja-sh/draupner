use anyhow::Result;

pub fn run() -> Result<()> {
    let target = crate::skills::default_target_root();
    crate::ui::header("ws-claude", &format!("target={}", target.display()));

    let s = crate::ui::step("skills", "writing SKILL.md files");
    let written = match crate::skills::write_all(&target) {
        Ok(w) => {
            s.ok();
            w
        }
        Err(e) => {
            s.fail(&format!("{e:#}"));
            return Err(e);
        }
    };

    let summary: Vec<String> = written
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    crate::ui::summary_ok(&summary);
    Ok(())
}
