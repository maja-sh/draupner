# workspaces

`ws` — a Rust CLI for git-worktree orchestration across multi-repo workspaces, with a plugin-style hook system for project-specific behavior.

## Build

```
cargo build --release
cargo nextest run 
```

Prereq for tests: `cargo install cargo-nextest --locked` (one-time, per machine).

## Environment variables

### Read by `ws`

| Var | Default | Purpose |
|-----|---------|---------|
| `WS_REMOTE_HOST` | — | Default SSH host for a bare `ws init --remote` (when no `<host>` is given). |
| `WS_SSH` | `ssh` | Override the `ssh` binary — e.g. point at a wrapper. Mirrors git's `GIT_SSH`. |
| `WS_MUTAGEN` | `mutagen` | Override the `mutagen` binary. |
| `CLAUDE_SKILLS_DIR` | `~/.claude/skills` | Where `ws claude` installs skill files. |
| `RUST_LOG` | `warn` | Tracing filter; quiet by default. `RUST_LOG=ws=debug ws <op>` for a full trace. |

`HOME` is read only to resolve the default skills directory.

`git` is intentionally **not** overridable: `ssh`/`mutagen` get an override seam
because they're the network/sync tools the (out-of-process) test suite must
stub; `git` always runs against real repositories.

### Set by `ws` for child processes

Hooks (`post-create`, `up`, `down`, `post-remove`, `status`) receive:
`WS_WORKSPACE_ROOT`, `WS_REPO`, `WS_BRANCH_SANITIZED`, `WS_HOOK_NAME`.

`ws exec` commands receive `WS_REPO` and `WS_BRANCH_SANITIZED` (identical on the
local and remote paths). Per-worktree routing vars (`WS_HOST_<svc>` /
`WS_PORT_<svc>`) arrive with the routing slice.
