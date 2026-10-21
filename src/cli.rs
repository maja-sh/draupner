use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "drup", version, about = "Multi-repo worktree orchestration")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bootstrap .drup.json + .drup-hooks/ in the current directory
    Init {
        /// Rewrite .drup.schema.json from the current schema, leave .drup.json alone
        #[arg(long)]
        refresh_schema: bool,
        /// Offload to a server: SSH host (falls back to $DRUP_REMOTE_HOST, then `drup config` remote.host)
        #[arg(long, value_name = "CONNECTION", num_args = 0..=1)]
        remote: Option<Option<String>>,
        /// Server dir the worktrees live under (default: this workspace's dir name)
        #[arg(long, value_name = "PATH", requires = "remote")]
        remote_path: Option<String>,
        /// Private TLD for per-worktree hostnames (routing; local or remote)
        #[arg(long, value_name = "DOMAIN")]
        domain: Option<String>,
        /// Pin the container runtime (default: detected on PATH, docker first)
        #[arg(long, value_name = "RUNTIME", value_parser = crate::config::RUNTIMES)]
        runtime: Option<String>,
        /// Skip the SSH reachability check (init refuses an unreachable host by default)
        #[arg(long, requires = "remote")]
        no_verify: bool,
    },
    /// Install Claude Code skill files (~/.claude/skills/drup-*)
    Claude,
    /// Create worktrees for a task
    New   {
        repos: String,
        branch: String,
        /// Bring the stack up immediately after creating the worktrees
        #[arg(long)]
        up: bool,
    },
    /// Add repo(s) to an existing task (bring the stack down first if it's up)
    Add   {
        repos: String,
        branch: String,
        /// Bring the task's stack up after adding
        #[arg(long)]
        up: bool,
    },
    /// Bring a task's stack up
    Up    { branch: String },
    /// Stop a task's stack
    Down  { branch: String },
    /// Remove a task's worktrees
    Rm {
        branch: String,
        #[arg(long)]
        force: bool,
        /// Keep the compose project's built images; the default prunes them alongside its volumes
        #[arg(long)]
        keep_images: bool,
        /// Keep the compose project's named volumes; the default drops them so recreating the branch starts clean
        #[arg(long)]
        keep_volumes: bool,
    },
    /// List active task worktrees
    List {
        /// One line per task: aggregate glyph + `running N/M`/`stopped`/`crashed`, no per-repo detail
        #[arg(short, long)]
        short: bool,
        /// Emit a JSON array on stdout — one object per task, same per-repo shape as `drup show --json`
        #[arg(long)]
        json: bool,
        /// Print the JSON schema and exit
        #[arg(long)]
        describe: bool,
    },
    /// Print a worktree path (composable with `$(...)`, e.g. `$EDITOR $(drup path feat/x -r api)`)
    Path {
        /// Branch of the task to look up
        branch: String,
        /// Narrow to one repo's worktree; without it, prints the task dir
        #[arg(short = 'r', long)]
        repo: Option<String>,
    },
    /// Show one worktree in detail (services, ports, hostnames, exec syntax)
    Show {
        /// Branch to describe; inferred from the cwd when omitted
        branch: Option<String>,
        /// Emit machine-readable JSON instead of the human summary
        #[arg(long)]
        json: bool,
        /// Print the JSON schema and exit (no branch needed)
        #[arg(long)]
        describe: bool,
    },
    /// Cwd-adaptive JSON wakeup for LLM agents (workspace, current, task, hints)
    Context {
        /// Print the JSON schema and exit
        #[arg(long)]
        describe: bool,
    },
    /// Audit worktrees; remove safe-to-drop ones (preview unless --force)
    Clean { #[arg(long, short)] force: bool },
    /// Ready this machine for routing (rootless bits directly; privileged bits consented)
    Setup {
        /// Persist a non-default HTTPS port (e.g. 443 for clean, suffix-free URLs)
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
        /// Persist a non-default Caddy admin port (move it off :2019 if a Caddy already runs there)
        #[arg(long, value_name = "PORT")]
        admin_port: Option<u16>,
        /// Run the privileged steps without prompting (for automation)
        #[arg(long)]
        yes: bool,
    },
    /// Configure or verify this workspace's remote offload
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
    /// Get/set machine-global defaults (~/.config/drup/config.toml)
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Run a command where the repo's stack lives (remote via ssh, else local)
    Exec {
        /// Repo to run in; inferred from the current worktree, else required (e.g. with --branch)
        repo: Option<String>,
        /// Worktree to run in, by branch; inferred from the cwd if omitted
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Force local execution even when the workspace is remote
        #[arg(long)]
        local: bool,
        /// Skip the pre-run mutagen flush
        #[arg(long)]
        no_flush: bool,
        /// The command to run, after `--`
        #[arg(last = true, required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Read-only health check of the offload server (or local routing readiness)
    Check {
        /// Server to check; overrides the workspace's configured remote
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Show the underlying commands each probe runs
        #[arg(long, short)]
        verbose: bool,
    },
    /// Offload this workspace to a server (writes the remote block + backfills sync)
    Set {
        /// SSH host (falls back to $DRUP_REMOTE_HOST, then `drup config` remote.host)
        connection: Option<String>,
        /// Server dir the worktrees live under (default: kept, or this workspace's dir name)
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Private TLD for per-worktree hostnames (routing; local or remote)
        #[arg(long, value_name = "DOMAIN")]
        domain: Option<String>,
        /// Skip the SSH reachability check (set refuses an unreachable host by default)
        #[arg(long)]
        no_verify: bool,
        /// Flip even with a stack running (it stays where drup can no longer reach it)
        #[arg(long)]
        force: bool,
    },
    /// Make this workspace local again (removes the remote block + sync sessions)
    Unset {
        /// Flip even with a stack running on the server (it stays there, unreachable)
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print a value (bare, on stdout; exit 1 when unset)
    Get { key: String },
    /// Set a value (e.g. `drup config set remote.host srv`)
    Set { key: String, value: String },
    /// Remove a value (a no-op when already unset)
    Unset { key: String },
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { refresh_schema, remote, remote_path, domain, runtime, no_verify } =>
            crate::ops::init::run(refresh_schema, remote, remote_path, domain, runtime, no_verify),
        Cmd::Claude                  => crate::ops::claude::run(),
        Cmd::New{repos,branch,up}    => crate::ops::new::run(&repos, &branch, up),
        Cmd::Add{repos,branch,up}    => crate::ops::add::run(&repos, &branch, up),
        Cmd::Up{branch}              => crate::ops::up::run(&branch),
        Cmd::Down{branch}            => crate::ops::down::run(&branch),
        Cmd::Rm{branch,force,keep_images,keep_volumes} => crate::ops::rm::run(&branch, force, keep_images, keep_volumes),
        Cmd::List{short,json,describe} => crate::ops::list::run(short, json, describe),
        Cmd::Path{branch,repo}       => crate::ops::path::run(&branch, repo.as_deref()),
        Cmd::Show{branch,json,describe} => crate::ops::show::run(branch.as_deref(), json, describe),
        Cmd::Context{describe}       => crate::ops::context::run(describe),
        Cmd::Clean{force}            => crate::ops::clean::run(force),
        Cmd::Setup{port,admin_port,yes} => crate::ops::setup::run(yes, port, admin_port),
        Cmd::Remote{cmd}             => match cmd {
            RemoteCmd::Check{host,verbose} => crate::ops::remote_check::run(host.as_deref(), verbose),
            RemoteCmd::Set{connection,path,domain,no_verify,force} =>
                crate::ops::remote_set::set(connection.as_deref(), path, domain, no_verify, force),
            RemoteCmd::Unset{force}      => crate::ops::remote_set::unset(force),
        },
        Cmd::Config{cmd}             => match cmd {
            ConfigCmd::Get{key}          => crate::ops::config::get(&key),
            ConfigCmd::Set{key,value}    => crate::ops::config::set(&key, &value),
            ConfigCmd::Unset{key}        => crate::ops::config::unset(&key),
        },
        Cmd::Exec{repo,branch,local,no_flush,command} =>
            crate::ops::exec::run(repo.as_deref(), branch.as_deref(), local, no_flush, &command),
    }
}
