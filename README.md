# ws

Multi-repo worktree orchestration.

> **Platform:** macOS is the only supported platform today. Linux support is
> on the roadmap.

One command creates a git worktree for the same branch across several repos,
brings each repo's stack up in isolation — its own containers, its own ports, its
own hostnames — and tears the whole thing down again.

The problem it solves: a feature that spans an API and a frontend means two
checkouts and two stacks, and the moment you want a second branch in flight they
fight over ports, container names, and database volumes. `ws` makes the *task*
the unit of work rather than the repo:

```sh
ws new api,app feat/checkout   # a worktree of feat/checkout in both repos
ws up feat/checkout            # both stacks up, de-conflicted
ws list                        # what's running, and whether it's actually serving
ws rm feat/checkout            # gone
```

Several tasks can run at once. Nothing is shared between them unless you say so.

---

## Install

Each release attaches a prebuilt binary for `aarch64-apple-darwin` (Apple Silicon):

```sh
gh release download vX.Y.Z --repo maja-sh/wsx -p '*aarch64-apple-darwin*' -O ws
chmod +x ws && mv ws ~/.local/bin/ws
```

Or from source (Rust stable):

```sh
cargo build --release   # → target/release/ws
```

### Prerequisites

On the machine you run `ws` from:

| Need | For |
|---|---|
| `git` | worktrees — always |
| `docker` or `podman` | `kind: container` repos — whichever is installed, docker first |
| `lsof` | reporting whether a host process is serving its port |
| `mutagen` | offloading a workspace to a server ([docs/offload.md](docs/offload.md)) |

Per-worktree **hostnames** are optional and need a little more: a wildcard DNS
answer for `*.<domain>` and a trusted local CA. `ws setup` reports what's
missing and offers to run each privileged step for you (showing the command
first — it never handles your password). On an offload server, the same ground
is covered by the readable script in [`server-setup/`](server-setup/). Without
a `domain` configured, `ws` skips routing entirely and services are reached on
allocated ports.

---

## Quickstart

A workspace is just a directory with your clones side by side:

```sh
mkdir ~/work/myapp && cd ~/work/myapp
git clone git@github.com:you/api.git
git clone git@github.com:you/app.git

ws init --domain myapp.test
```

`ws init` detects the repos, derives what it can from each one's compose file,
and writes three things:

- **`.ws.json`** — the workspace config. Yours to edit.
- **`.ws.schema.json`** — a JSON Schema for it, generated from the code, so your
  editor completes and validates every field. Regenerate after an upgrade with
  `ws init --refresh-schema`.
- **`.ws-hooks/`** — starter scripts for the lifecycle events you want to fill
  in. Each is inert until a repo's `scripts` block in `.ws.json` names it.

Then work a task:

```sh
ws new api,app feat/checkout    # worktrees + post-create hooks
ws up feat/checkout             # stacks up, routes installed, ports reported
ws exec api -- php artisan tinker
ws down feat/checkout           # stop, keep the files
ws rm feat/checkout             # stop and remove the worktrees
```

`ws list` shows every task, and `ws clean` audits worktrees and offers to drop
the ones that are merged and clean.

---

## The model

**Workspace** — a directory holding your repo clones plus `.ws.json`. The clones
stay on their default branch and `ws` never commits in them; it reads them and
creates worktrees from them.

**Task** — a branch name. Every repo you name gets a worktree of that branch, and
the branch is what you pass to every later command.

**Worktree** — `<workspace>/.worktrees/<branch>/<repo>/`. A real `git worktree`,
so `git` is the source of truth about what exists. Branch names are sanitized
into one path/DNS-safe label (`feat/checkout` → `feat-checkout`) which then names
the task dir, the compose project, and the hostname prefix.

**Stack** — what a repo runs for a task. Two kinds:

- `kind: container` — a compose project. `ws` generates a per-worktree override
  that renames the project and reissues published ports, so N branches of the
  same repo coexist.
- `kind: host` — a long-running process (`vite`, `cargo watch`, `artisan serve`)
  that `ws` starts detached, tracks by PID file, and can report the health of.
  No systemd or launchd, so it works the same on macOS.

**Isolation** is per service. A raw-TCP service (a database, redis) gets its own
allocated host port, recorded so it stays stable across `up` runs. An HTTP
service instead gets a *hostname* — `<branch>.<subdomain>.<domain>` — routed by a
shared Caddy, so any number of branches share one HTTPS port. That split is why
`ws` cares whether a service is HTTP: hostnames multiplex, raw TCP can't.

---

## Commands

| Command | What it does |
|---|---|
| `ws init` | Scaffold `.ws.json`, `.ws.schema.json`, `.ws-hooks/` from the repos found here |
| `ws new <repos> <branch>` | Create worktrees for a task (comma-separated repos); `--up` to start immediately |
| `ws up <branch>` | Bring the task's stacks up, install routes, report ports |
| `ws down <branch>` | Stop the stacks, leave the files |
| `ws rm <branch>` | Stop and remove the worktrees (`--force` when dirty) |
| `ws list` | Every task, its repos, and their run state |
| `ws clean` | Audit worktrees; preview what's safe to drop, `--force` to act |
| `ws exec [repo] -- <cmd>` | Run a command where that repo's stack lives (over SSH when offloaded) |
| `ws setup` | Ready this machine for routing; reports the gaps it can't close itself |
| `ws remote check` | Read-only health check of the offload server (or of local routing) |
| `ws remote set/unset` | Move this workspace to a server, or bring it back; refuses while a stack is running |
| `ws config get/set/unset` | Machine-global defaults in `~/.config/ws/config.toml` |
| `ws claude` | Install Claude Code skills for the `ws` workflow |

Every command takes `--help`.

---

## Documentation

- **[docs/configuration.md](docs/configuration.md)** — `.ws.json`: services and
  routing, how compose derivation works, and the precedence rules. The field
  list lives in `.ws.schema.json`; this covers what a schema can't say.
- **[docs/hooks.md](docs/hooks.md)** — the lifecycle events, where each one runs,
  and the full `WS_*` environment contract your scripts receive.
- **[docs/offload.md](docs/offload.md)** — running the stacks on a server while
  you edit locally.
- **[server-setup/](server-setup/)** — the one-time privileged host setup.

---

## Environment

Read by `ws`:

| Var | Default | Purpose |
|-----|---------|---------|
| `WS_REMOTE_HOST` | — | Default SSH host for a bare `ws init --remote` / `ws remote set`. Precedence: explicit argument > this > `ws config` `remote.host` |
| `WS_SSH` | `ssh` | Override the `ssh` binary — e.g. point at a wrapper. Mirrors git's `GIT_SSH` |
| `WS_MUTAGEN` | `mutagen` | Override the `mutagen` binary |
| `CLAUDE_SKILLS_DIR` | `~/.claude/skills` | Where `ws claude` installs skill files |

`HOME` is read to resolve `~/.config/ws` and the default skills directory.

`git` is deliberately **not** overridable: `ssh`/`mutagen` get an override seam
because they're the network and sync tools the out-of-process test suite has to
stub; `git` always runs against real repositories.

Two more exist as escape hatches rather than everyday settings.
`WS_HTTPS_PORT` overrides the port Caddy serves on for one invocation (the
durable way is `ws setup --port`). `WS_CADDY_ADMIN` points `ws` at an admin API
you manage yourself, which also stops it from creating the network or running a
Caddy container at all — bring-your-own routing.

---

## Development

```sh
cargo build
cargo test
```

Tests run the real mechanisms — real git repos, real worktrees, real detached
processes — in temporary fixtures with `$HOME` redirected. A few are gated on an
external binary (`docker compose`, `lsof`, `python3`) and skip with a note
rather than failing when it isn't there.

Releases are cut by pushing a version tag; CI builds the macOS binary natively
and attaches it to the release.
