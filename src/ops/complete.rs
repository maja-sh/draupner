//! Hidden `drup __complete <kind>` — enumerates values for shell completion.
//!
//! One name per line on stdout, no decoration. Silent on error so a shell
//! tab-press outside a workspace never prints a scary message into the user's
//! prompt area; the completer just offers no matches.

use anyhow::Result;
use std::collections::BTreeSet;

pub fn run(kind: &str) -> Result<()> {
    let Ok(ws) = crate::workspace::locate() else { return Ok(()) };
    let Ok(cfg) = crate::config::load(&ws.root) else { return Ok(()) };
    match kind {
        "branch" => {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for wt in crate::workspace::worktrees(&ws.root, &cfg) {
                if let Some(b) = wt.branch {
                    names.insert(b);
                }
            }
            for name in names {
                println!("{name}");
            }
        }
        "repo" => {
            for entry in &cfg.repos {
                println!("{}", entry.name());
            }
        }
        _ => {}
    }
    Ok(())
}
