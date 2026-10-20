//! `drup` — multi-repo git-worktree orchestration.
//!
//! The `drup` binary treats a task (one branch across N repos) as the unit of work:
//! it spawns matching worktrees, brings each repo's stack up with de-conflicted
//! ports and hostnames, and tears the whole set down again. See the README for
//! usage; module-level docs describe internals.

mod caddy;
mod cli;
mod compat;
mod compose;
mod config;
mod git;
mod hooks;
mod inject;
mod mutagen;
mod ops;
mod ports;
mod probe;
mod remote;
mod setup;
mod signal;
mod skills;
mod supervise;
mod ui;
mod workspace;

fn main() {
    compat::check();
    signal::install();
    // `__complete <kind>` is the shell-completion callback. Dispatched before
    // clap parses so the subcommand never enters the clap command tree and
    // clap_complete can't leak it into the generated scripts (4.6.11 emits
    // every subcommand regardless of `hide = true`).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__complete") {
        if let Some(kind) = args.get(2) {
            let _ = ops::complete::run(kind);
        }
        return;
    }
    if let Err(e) = cli::run() {
        let exit = e.downcast_ref::<ops::ExitError>();
        let code = exit.map_or(1, |e| e.code);
        // A silent passthrough (e.g. `drup exec` mirroring a child's exit code)
        // carries its own code but should print no banner over the command's
        // own output. Everything else reports normally.
        if !exit.is_some_and(|e| e.silent) {
            crate::ui::summary_fail(&format!("{e:#}"));
        }
        std::process::exit(code);
    }
}
