use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const SKILLS: &[(&str, &str)] = &[
    ("ws-new",      include_str!("../skills/ws-new/SKILL.md")),
    ("ws-up",       include_str!("../skills/ws-up/SKILL.md")),
    ("ws-down",     include_str!("../skills/ws-down/SKILL.md")),
    ("ws-rm",       include_str!("../skills/ws-rm/SKILL.md")),
    ("ws-list",     include_str!("../skills/ws-list/SKILL.md")),
    ("ws-here",     include_str!("../skills/ws-here/SKILL.md")),
    ("ws-use",      include_str!("../skills/ws-use/SKILL.md")),
    ("ws-clean",    include_str!("../skills/ws-clean/SKILL.md")),
    ("ws-init",     include_str!("../skills/ws-init/SKILL.md")),
];

pub fn write_all(target_root: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(target_root)
        .with_context(|| format!("create {}", target_root.display()))?;
    let mut written = Vec::with_capacity(SKILLS.len());
    for (name, body) in SKILLS {
        let dir = target_root.join(name);
        fs::create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join("SKILL.md");
        fs::write(&path, body)
            .with_context(|| format!("write {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// Where skills are written by default. Overridable via `CLAUDE_SKILLS_DIR` for tests.
pub fn default_target_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_SKILLS_DIR") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME")
        .context("HOME is not set — set CLAUDE_SKILLS_DIR or run in a normal user environment")?;
    Ok(PathBuf::from(home).join(".claude").join("skills"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skill_has_frontmatter_and_invoke_line() {
        for (name, body) in SKILLS {
            assert!(body.starts_with("---\n"), "{name}: missing frontmatter");
            assert!(body.contains(&format!("name: {name}")), "{name}: frontmatter name mismatch");
            assert!(body.contains("description:"), "{name}: missing description");
            assert!(body.contains("Invoke via bash"), "{name}: missing invoke line");
        }
    }

    #[test]
    fn write_all_creates_every_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let written = write_all(tmp.path()).unwrap();
        assert_eq!(written.len(), SKILLS.len());
        for (name, _) in SKILLS {
            assert!(tmp.path().join(name).join("SKILL.md").is_file(), "missing {name}");
        }
    }

    #[test]
    fn write_all_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        write_all(tmp.path()).unwrap();
        write_all(tmp.path()).unwrap();
        assert!(tmp.path().join("ws-new").join("SKILL.md").is_file());
    }
}
