//! `drup completion <shell>` — print a shell-completion script to stdout.
//!
//! The script is clap_complete's output for the given shell, with dynamic
//! completers injected for branch and repo arguments: tab-pressing on a
//! branch slot calls back into `drup __complete branch` to list live
//! worktree branches, and likewise for repos.

use anyhow::Result;
use clap_complete::{generate, Shell};

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = crate::cli::command();
    let mut buf: Vec<u8> = Vec::new();
    generate(shell, &mut cmd, "drup", &mut buf);
    let script = String::from_utf8(buf)?;
    let out = match shell {
        Shell::Zsh => inject_zsh(&script),
        Shell::Fish => inject_fish(&script),
        _ => script,
    };
    print!("{out}");
    Ok(())
}

/// Zsh: install `_drup_branches`/`_drup_repos` and swap clap's `_default`
/// action for them on every branch/repo slot where the completion makes
/// sense. `new`'s positional branch is a *new* name — leaving it on
/// `_default` keeps tab from falsely suggesting existing worktrees.
fn inject_zsh(script: &str) -> String {
    let helpers = r#"
_drup_branches() {
    local -a names
    names=(${(f)"$(drup __complete branch 2>/dev/null)"})
    _describe 'branch' names
}

_drup_repos() {
    local -a names
    names=(${(f)"$(drup __complete repo 2>/dev/null)"})
    _describe 'repo' names
}
"#;
    let mut out = match script.find("\n_drup()") {
        Some(i) => {
            let mut s = String::with_capacity(script.len() + helpers.len());
            s.push_str(&script[..i]);
            s.push_str(helpers);
            s.push_str(&script[i..]);
            s
        }
        None => format!("{script}{helpers}"),
    };

    // Option values render `:METAVAR:_default` — safe to swap globally since
    // the metavars are specific enough.
    out = out
        .replace(":BRANCH:_default", ":BRANCH:_drup_branches")
        .replace(":REPO:_default", ":REPO:_drup_repos");

    // Positionals with a help string render `':branch -- help:_default'`;
    // the help text is specific to each arm, so pass through unchanged on
    // everything that isn't an explicit opt-out.
    out = swap_positional_default_with_help(&out, "branch", "_drup_branches");
    out = swap_positional_default_with_help(&out, "repo", "_drup_repos");

    // Positionals without a help string render `':branch:_default'` and are
    // ambiguous across subcommands — swap them per-arm.
    for name in ["up", "down", "rm", "path", "show", "add"] {
        out = patch_arm(&out, name, ":branch:_default", ":branch:_drup_branches");
    }
    for name in ["path", "exec"] {
        out = patch_arm(&out, name, ":repo:_default", ":repo:_drup_repos");
    }

    out
}

/// Replace every `':<arg> -- <help>:_default'` with `':<arg> -- <help>:<action>'`,
/// leaving other `_default` sites untouched.
fn swap_positional_default_with_help(script: &str, arg: &str, action: &str) -> String {
    let anchor = format!(":{arg} -- ");
    let mut out = String::with_capacity(script.len());
    for line in script.split_inclusive('\n') {
        if line.contains(&anchor) && line.contains(":_default") {
            out.push_str(&line.replacen(":_default", &format!(":{action}"), 1));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Replace `old` with `new` inside the zsh case arm that starts with `(name)`
/// and ends at the next `;;`. Arms we care about are top-level subcommand
/// names that don't recur as nested arms (`check`/`set`/`get`/`unset`/`help`
/// are the only names that recur, and none of them are in the caller's list).
fn patch_arm(script: &str, name: &str, old: &str, new: &str) -> String {
    let open = format!("({name})\n");
    let Some(open_at) = script.find(&open) else {
        return script.to_string();
    };
    let body_at = open_at + open.len();
    let rest = &script[body_at..];
    let close_rel = rest.find("\n;;\n").map(|i| i + 1).unwrap_or(rest.len());
    let body = &rest[..close_rel];
    let patched = body.replace(old, new);
    let mut out = String::with_capacity(script.len());
    out.push_str(&script[..body_at]);
    out.push_str(&patched);
    out.push_str(&rest[close_rel..]);
    out
}

/// Fish: append `complete -c drup` lines that call `drup __complete` for
/// branch/repo-shaped slots across every subcommand that takes one.
fn inject_fish(script: &str) -> String {
    let branch_subs = "up down rm path show add";
    let repo_subs = "exec";
    let mut extra = String::from("\n");
    extra.push_str(&format!(
        "complete -c drup -n \"__fish_seen_subcommand_from {branch_subs}\" -f -a \"(drup __complete branch)\"\n"
    ));
    extra.push_str(
        "complete -c drup -n \"__fish_seen_subcommand_from exec\" -s b -l branch -r -f -a \"(drup __complete branch)\"\n"
    );
    extra.push_str(&format!(
        "complete -c drup -n \"__fish_seen_subcommand_from {repo_subs}\" -f -a \"(drup __complete repo)\"\n"
    ));
    extra.push_str(
        "complete -c drup -n \"__fish_seen_subcommand_from path\" -s r -l repo -r -f -a \"(drup __complete repo)\"\n"
    );
    format!("{script}{extra}")
}
