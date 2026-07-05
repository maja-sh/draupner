use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ws", version, about = "Multi-repo worktree orchestration")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bootstrap .ws.json + .ws-hooks/ in the current directory
    Init {
        /// Rewrite .ws.schema.json from the current schema, leave .ws.json alone
        #[arg(long)]
        refresh_schema: bool,
    },
    /// Install Claude Code skill files (~/.claude/skills/ws-*)
    Claude,
    /// Create worktrees for a task
    New   { repos: String, branch: String },
    /// Bring a task's stack up
    Up    { branch: String },
    /// Stop a task's stack
    Down  { branch: String },
    /// Remove a task's worktrees
    Rm    { branch: String, #[arg(long)] force: bool },
    /// List active task worktrees
    List,
    /// Print worktree paths for the current task
    Here,
    /// Audit worktrees; remove safe-to-drop ones (preview unless --force)
    Clean { #[arg(long, short)] force: bool },
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { refresh_schema } => crate::ops::init::run(refresh_schema),
        Cmd::Claude                  => crate::ops::claude::run(),
        Cmd::New{repos,branch}       => crate::ops::new::run(&repos, &branch),
        Cmd::Up{branch}              => crate::ops::up::run(&branch),
        Cmd::Down{branch}            => crate::ops::down::run(&branch),
        Cmd::Rm{branch,force}        => crate::ops::rm::run(&branch, force),
        Cmd::List                    => crate::ops::list::run(),
        Cmd::Here                    => crate::ops::here::run(),
        Cmd::Clean{force}            => crate::ops::clean::run(force),
    }
}
