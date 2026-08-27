use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const SKILLS: &[(&str, &str)] = &[
    ("ws-new",      include_str!("../skills/ws-new/SKILL.md")),
    ("ws-up",       include_str!("../skills/ws-up/SKILL.md")),
    ("ws-down",     include_str!("../skills/ws-down/SKILL.md")),
    ("ws-rm",       include_str!("../skills/ws-rm/SKILL.md")),
    ("ws-list",     include_str!("../skills/ws-list/SKILL.md")),
    ("ws-clean",    include_str!("../skills/ws-clean/SKILL.md")),
    ("ws-show",     include_str!("../skills/ws-show/SKILL.md")),
    ("ws-exec",     include_str!("../skills/ws-exec/SKILL.md")),
    ("ws-resume",   include_str!("../skills/ws-resume/SKILL.md")),
];

/// Skills that used to ship and were removed. `write_all` sweeps them from the target so a re-run of `ws claude` cleans up any prior install. Keep entries here for at least one release cycle after retirement so users who upgrade catch the removal; then drop them.
const DEPRECATED_SKILLS: &[&str] = &["ws-init"];

pub fn write_all(target_root: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(target_root)
        .with_context(|| format!("create {}", target_root.display()))?;
    // Best-effort — a missing dir is the common case; only a real fs error would matter and it'd resurface on the write below.
    for name in DEPRECATED_SKILLS {
        let _ = fs::remove_dir_all(target_root.join(name));
    }
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
    fn every_skill_has_valid_frontmatter() {
        for (name, body) in SKILLS {
            assert!(body.starts_with("---\n"), "{name}: missing frontmatter");
            assert!(body.contains(&format!("name: {name}")), "{name}: frontmatter name mismatch");
            assert!(body.contains("description:"), "{name}: missing description");
        }
    }

    #[test]
    fn command_wrapper_skills_carry_an_invoke_line() {
        // Most skills just map `/ws-<x>` to `ws <x>`; a few (ws-resume) are
        // procedures with no single command, so the invoke line is not universal.
        for (name, body) in SKILLS {
            if *name == "ws-resume" {
                continue;
            }
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

    #[test]
    fn write_all_sweeps_deprecated_skills_from_prior_installs() {
        let tmp = tempfile::tempdir().unwrap();
        // A prior `ws claude` install had a skill that has since been retired — the file below stands in for whatever `~/.claude/skills/<retired>/SKILL.md` a user carried over from an older ws.
        for name in DEPRECATED_SKILLS {
            let dir = tmp.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("SKILL.md"), "stale").unwrap();
        }
        write_all(tmp.path()).unwrap();
        for name in DEPRECATED_SKILLS {
            assert!(
                !tmp.path().join(name).exists(),
                "deprecated skill `{name}` must be swept when `ws claude` runs against an older install"
            );
        }
    }
}
