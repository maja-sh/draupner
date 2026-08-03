# Architecture

For someone changing `ws`. If you want to *use* it, start at the
[README](README.md) and [docs/](docs/) — this page deliberately doesn't repeat
them.

`ws` is ~9k lines of Rust across 38 files, but it is four ideas and one call
chain. This page names both, then traces `ws up` end to end, so the rest of the
tree reads as variations on a path you have already seen.

---

## Start here

Read in this order. It is about an hour, and everything else follows from it.

1. **`src/config.rs`, top to bottom.** It is the vocabulary every other module
   speaks. `WsConfig`, `RepoConfig`, `ServiceConfig`, `RepoKind`, `Script` are
   the nouns. Their doc comments are compiled by `schemars` into
   `.ws.schema.json` and shipped to users (`ops/init.rs`), so they are product
   documentation, not internal notes. The guards at the bottom of `load` are
   there because `.ws.json` is not gitignored and therefore arrives with a
   clone — see *Deviations* below.
2. **`src/remote.rs`, the first ~230 lines.** That is `Site`, and nothing below
   it matters yet. `Site` is why there is no separate remote implementation of
   anything.
3. **`src/inject.rs` — the module header and `prepare`.** This is the hub: it is
   where `.ws.json`, compose derivation, port allocation, template rendering and
   Caddy routes all meet. Every op that touches a worktree's stack calls
   `inject::prepare`.
4. **`src/ops/hooksweep.rs::sweep`.** The actual body of both `ws up` and
   `ws down`. `ops/up.rs` is 24 lines of summary formatting; the name
   `hooksweep` is the least accurate one in the tree and the file a newcomer
   needs most.
5. **`src/lifecycle.rs`, all 290 lines.** One `Ctx`, one dispatch, two sites.
   Small, and it is the piece that makes step 4 readable.
6. **Then the one subsystem your change touches.** Their module headers are
   genuinely good — `caddy.rs`, `supervise.rs`, `setup.rs`, `ports.rs`,
   `compose.rs` each open by explaining why they are shaped the way they are.
   Read the header before the code.

Then trace `ws up` below against the real files.

---

## The shape

```
main.rs          compat::check → signal::install → cli::run
  └ cli.rs       clap parses one subcommand
      └ ops/<verb>.rs        one module per CLI subcommand
          ├ workspace::locate()      find .ws.json by walking up
          ├ config::load()           parse + boundary guards
          ├ remote::Site::resolve()  local, or the server — decided ONCE
          ├ inject::prepare()        ports, hostnames, env, templates, override
          ├ lifecycle::Ctx           run this event, wherever the stack lives
          └ {caddy, compose, ports, supervise, mutagen}   ← all take a &Site
```

`main` does four things and hands off. Ops never `println!` — every line of
output goes through `ui`. Errors are `anyhow`; an op that wants a specific exit
code returns `ops::ExitError` (`preflight` = 2, `op_failure` = 1,
`bail_if_interrupted` = 130, `passthrough_exit` = silent).

### One line per module

**Ops** — one per subcommand, plus two shared drivers:

| File | Owns |
|---|---|
| `ops/hooksweep.rs` | **The body of `ws up` and `ws down`.** Per-repo sweep, routing preflight, host-process launch, route sync, port forwards |
| `ops/teardown.rs` | The body of `ws rm` and `ws clean` — `down` + `post-remove`, worktree removal, branch deletion |
| `ops/up.rs`, `ops/down.rs` | 24 lines each: call `sweep`, render the summary |
| `ops/new.rs` | Worktree creation, `copy`/`vendor` seeding, mutagen session, `post-create` |
| `ops/rm.rs`, `ops/clean.rs` | The *gating* around teardown (dirty/unpushed check; merged/clean audit) |
| `ops/list.rs` | The table: git enumeration + `status` capture + host `Health` |
| `ops/exec.rs` | Run a command where a repo's stack lives; passes the child's exit code through |
| `ops/init.rs` | Scaffold `.ws.json`, `.ws.schema.json`, `.ws-hooks/` |
| `ops/setup.rs`, `ops/remote_check.rs`, `ops/remote_set.rs` | Thin fronts: resolve the routing address and hand off to `setup.rs`; probe a server; flip a workspace between local and offloaded |
| `ops/config.rs`, `ops/claude.rs` | `~/.config/ws/config.toml`; install the Claude Code skills |

**Services** — cross-cutting, called by ops, never the other way round:

| File | Answers |
|---|---|
| `remote.rs` | *Where does this run?* `Site`, plus the SSH plumbing behind it |
| `lifecycle.rs` | *Run this repo's `<event>` for this worktree* — dispatches on `Site` |
| `inject.rs` | *What are this worktree's services, ports, hostnames, env, templates, routes?* |
| `hooks.rs` | *What command does `(repo, event)` run,* and run it **here** |
| `ports.rs` | Host-port allocation, persisted in `.ws-meta/`. The record **is** the reservation |
| `compose.rs` | Derive services from `compose config`; render the per-worktree override |
| `caddy.rs` | The containerized Caddy on `ws-net` and its admin-API route CRUD |
| `supervise.rs` | `kind: host` processes: detached launch, PID-file identity, `Health` |
| `mutagen.rs` | Sync sessions (files) and forward sessions (ports) for offloaded workspaces |
| `setup.rs` | Routing readiness probe + the consented privileged fixes |
| `probe.rs` | Read-only server checks behind `ws remote check`. Never mutates, never `sudo`s |
| `workspace.rs` | The on-disk layout, branch sanitization, and *what counts as a worktree* |
| `config.rs` | The `.ws.json` schema and the load-time guards that make it safe to act on |
| `settings.rs` | Machine-global defaults under `~/.config/ws/` — the file behind `ws config` |
| `git.rs` | The repeated `git -C <path> …` invocations. Policy stays in the ops |

**Plumbing** — read once, forget:

`ui.rs` (the whole visual language), `signal.rs`
(cooperative Ctrl-C), `compat.rs` (macOS floor warning, matrix baked in by
`build.rs`), `skills.rs` (the `ws claude` payload).

---

## `ws up feat/checkout`, traced

A local workspace, one `kind: container` repo, `domain` set. Every other command
is a subset of this.

**`main.rs` → `cli.rs` → `ops/up.rs` → `ops/hooksweep::sweep(branch, "up", "ws-up")`.**
`up.rs` does nothing but call `sweep` and turn the returned `SweepResult` into a
summary; a non-empty `failed` becomes `op_failure` and exit 1.

Inside `sweep`:

1. **`workspace::locate()`** walks up from the cwd for `.ws.json`.
   **`config::load(&ws.root)`** parses it and applies the boundary guards.
   **`workspace::sanitize_branch("feat/checkout")`** → `feat-checkout`, the one
   label that names the task dir, the compose project and the hostname prefix.

2. **`Site::resolve(&cfg)`** → `Site::Local`, because there is no `remote` block.
   This is the *only* place local-vs-remote is decided. Nothing below branches on
   it again.

3. **Targets**: repos in `.ws.json` order that actually have
   `.worktrees/feat-checkout/<repo>` (`workspace::worktree_for`). None ⇒
   `preflight` (exit 2, "run `ws new` first"). Then `ui::header("ws-up", branch)`.

4. **Phase**: `up` ⇒ `inject::Phase::Activate`. (`down` ⇒ `Inspect`.)

5. **Routing preflight**, once, before the loop, when `cfg.routing_active()`:
   `caddy::publish_address` picks the bind address, then
   **`caddy::ensure_routing`** creates `ws-net`, runs the `ws-caddy` container,
   waits for its admin API, and probes whether `*.<domain>` resolves. Failure
   here is a **note, not fatal** — a sweep of raw-TCP-only repos needs neither
   Caddy nor `ws-net`. Fatality lands where routing is actually load-bearing, in
   step 11. A missing privileged bit (DNS, CA trust) triggers a `ws setup` nudge
   via `setup::probe`.

Then, per repo:

6. **`run_prepare`** builds a `lifecycle::Ctx` pinned to `Site::Local` and asks
   for the `prepare` command. `prepare` is the one event that always runs on the
   client — see [docs/hooks.md](docs/hooks.md). No `prepare` script ⇒ silent
   `true` and on we go.

7. **`cfg.is_host_repo(repo)`?** For a `kind: host` repo the whole rest of this
   is replaced by `sweep_host` (step 12). Ours is a container, so:

8. **`lifecycle::Ctx { cfg, site, root, worktree, branch, sanitized, repo }`**,
   then **`ctx.command("up")`** → **`hooks::resolve`**, one ladder:
   the declared `scripts.up` (via `config::script_command`, which joins a `Many`
   fail-fast with `&&` and anchors a `.ws-hooks/…` path to `$WS_WORKSPACE_ROOT`),
   else a refusal if a legacy executable `.ws-hooks/<repo>/up` is lying around,
   else the container default `podman compose up -d`. `None` ⇒ `ui::skip`, filed
   under `skipped`.

9. **`ctx.flush_to_server()`** — a no-op locally. **`inject::prepare(…,
   Phase::Activate)`** is where the real work is:

   - `workspace_context` → `WS_RUNTIME`, `WS_REMOTE`, `WS_CONNECTION`.
   - `Site::worktree_at` → the worktree path *at the execution site* (here, the
     local one).
   - `Phase::activating()` ⇒ **`effective_services`** runs
     `compose::derive` = `<runtime> compose config --format json` at the site,
     one `ServiceConfig` per published `(service, port)`, HTTP only for container
     port 80/443. The declared `services` block is overlaid on top, key by key.
   - **`ports::resolve`** allocates a host port per raw-TCP service into
     `.ws-meta/feat-checkout-api.json`, under an `O_EXCL` lock, avoiding every
     port recorded anywhere in `.ws-meta`, test-binding each candidate in
     `20000..40000`. Existing allocations are preserved — that is what keeps a
     connection string stable across runs.
   - **`ports::record_services`** writes the effective set into the same record,
     so the read-only phases below can reconstruct hosts and ports for services
     that were derived and never declared.
   - **`ws_vars`** builds `WS_HOST_<svc>` = `<branch>.<subdomain>.<domain>` and
     `WS_PORT_<svc>`. `peer_vars` adds every *other* repo's, re-keyed
     `WS_PORT_<peer>.<svc>`, for substitution only.
   - `Phase::activating()` ⇒ **`render_all`** renders each declared template
     (`ensure_within_worktree` on both paths, refuse a git-tracked dest,
     `substitute`), then registers the dests in `.git/info/exclude`.
   - `declared_env` applies the repo's `env` map with `${WS_*}` substituted.
   - Container ⇒ `COMPOSE_PROJECT_NAME = compose::project_name(...)` on every
     phase, and on an activating phase **`compose::write_override`** renders
     `.ws-meta/feat-checkout-api.override.yml` via `Site::write_file`: HTTP
     services get `ports: !reset []` and join `ws-net`, raw-TCP services get
     `ports: !override` with the allocated host port, declared `env` becomes
     `environment:`. If a file was written, `COMPOSE_FILE=<base>:<override>`.

   The returned `Vec<(String, String)>` is the injection env.

10. **`ctx.run("up", &cmd, &extra_env)`** → the `Site::Local` arm →
    **`hooks::run_command`** → `sh -c <cmd>` with cwd = the worktree,
    `hooks::base_env` plus the injection env, stdout and stderr streamed on two
    threads through `ui::hook_line`. Non-zero ⇒ `"up script exited N"`.
    `SweepResult::record` closes the step line and files the repo.

11. **`route_after_hook` → `sync_routes`** → `inject::routes` (read-only; it uses
    the *recorded* service set, so `down` and `rm` resolve the same `@id`s `up`
    created) → **`caddy::route_up`** per HTTP service: DELETE `/id/<id>/`, then
    PUT the fresh route at index `0` of the `ws` server's route list, ahead of the
    catch-all stopped-stack page. Never `POST /load` — that would clobber every
    other worktree.

    A failed route ⇒ **`SweepResult::demote`** moves the repo from `ran` to
    `failed`. That is not pedantry: the override `!reset` away the host publish,
    so Caddy is the only path in, so a container without its route is not up.

12. **`sweep_host`** (the `kind: host` branch of step 7) is the same story with a
    different runner: the declared `up` script is launched **detached** by
    `supervise::up` (`nohup env … sh -c … &`, pid + `ps -o lstart` recorded to
    `~/.ws-run/<name>.pid`), then `supervise::health_settled` waits up to
    `BIND_GRACE` and reports a `Health`. Routes are best-effort here, not
    demoting, because a host process keeps its real `WS_PORT` on the host and
    stays reachable there.

13. **`sync_forwards`** is a no-op for a local workspace.

Back in `ops/up.rs`: `Up: api` / `No up hook: docs`, `ui::summary_ok`.

### What changes when the workspace is offloaded

Only five things, all of them inside abstractions the trace above already
crossed:

- Step 2 dials the server for `$HOME` (so an unreachable server fails here, with
  a clear message, before anything is created).
- Step 9's `ctx.flush_to_server()` stops being a no-op — it flushes mutagen so the
  derive in `inject::prepare` reads the *server's* copy of the compose file.
- `inject::prepare`'s derive, port-freeness probe and override write happen at the
  site. The port *record* and its lock stay on the client, which is the only
  machine that runs `ws`.
- Step 10 takes the `Site::Remote` arm: `Ctx::push_to_server` (flush the sync,
  resolve the server root, `tar | ssh tar -x` the workspace-root `.ws-hooks/`),
  then `remote::run_event` — `cd <worktree> && env K=V … exec sh -c <cmd>` over
  SSH, with the login `PATH` harvested from the user's interactive shell so
  nvm/asdf toolchains resolve.
- Step 13 creates a `mutagen forward` session per allocated port, so every
  server-bound port answers at the same `localhost:<port>` here.

---

## The concepts

**`Site`** (`remote.rs`) — where a workspace's stack runs. `Local`, or
`Remote { connection, home }` with the server's `$HOME` resolved once at
construction. Both variants honour one contract, which is why `caddy`, `compose`,
`ports` and `supervise` take a `&Site` and never ask whether they are local.
Resolved exactly once per op. See *Local and remote are one path* below.

**`Phase`** (`inject.rs`) — what an event is allowed to do to the world, as a
ladder: `Activate` (`up`, `post-create`) does everything; `Exec` (local
`ws exec`) only allocates ports; `Inspect` (`down`, `post-remove`, `status`,
`list`, remote `exec`) does nothing but read. Two predicates: `allocates()` is
true for `Activate` and `Exec`, `activating()` only for `Activate` — and
`activating` is the one that gates deriving services, rendering templates and
writing the compose override, because all three shell out or touch the tree.

It exists so callers state intent rather than remembering to pair a render call
with an env call. `Exec` is the interesting one: it needs `WS_PORT_*` present, so
it allocates, but it is a command runner and must not rewrite config.

**`RepoKind`** (`config.rs`) — `Container` or `Host`, and the asymmetry is
deliberate. `is_container` infers from a compose file when `kind` is unset;
`is_host` requires the explicit `kind: host`, because supervision turns `up` from
a blocking script into a detached process and that should never happen by
accident.

**`ServiceConfig`** (`config.rs`) — a thing that listens, plus whether it is
HTTP. That one bit decides everything downstream: HTTP gets a hostname
(`subdomain()`) and multiplexes through Caddy; raw TCP gets an allocated host
port because there is no Host header to route on. A `kind: host` repo gets a port
for *every* service including HTTP ones — a process has to bind something.

**`Script`** (`config.rs`) — one shell command or a list. `joined()` is the single
owner of "what shell string does this event run": it joins fail-fast with `&&`
and anchors `.ws-hooks/…` to `$WS_WORKSPACE_ROOT`. Deferring either rule to the
dispatch site is how the captured (`ws list`) path once joined without anchoring.

**`Health`** (`supervise.rs`) — what a host process is *actually* doing, as
distinct from whether a pid is alive. `Serving` / `Silent` / `Squatted` /
`Crashed` / `NotStarted` / `Running`. It exists because a dev server can be
perfectly alive on a port `ws` never allocated (vite silently increments past a
busy one), and the port `ws` did allocate can be held by an orphan — both read as
healthy to a pid check, and both surface much later somewhere unrelated.

**`SweepResult`** (`ops/hooksweep.rs`) — `ran` / `skipped` / `failed`, plus the
`record`, `inject_env` and `demote` helpers. It is what makes per-repo failure
isolation the default: one repo's broken hook doesn't strand the others, and the
caller decides the exit code from the three lists.

**`lifecycle::Ctx`** — the worktree a lifecycle event runs for, and where its
stack lives. `command()` resolves *what* to run (one ladder, identical at both
sites); `run()` / `capture()` dispatch it. It replaced four hand-written
`if site.is_remote()` forks. The remote-only steps (flush, `.ws-hooks/` push,
`PATH` harvest) live *inside* the `Remote` arm on purpose — that is the
abstraction working, not leaking.

---

## Local and remote are one path

Three things are genuinely site-specific, and nothing else is.

**`Site` itself** — `command`/`shell`/`write_file` run here or over SSH, and
`root_at`/`worktree_at` translate client paths into server paths. Server paths
must be **absolute**: a lifecycle command runs with `cd <worktree>`, so a
relative `COMPOSE_FILE` or `WS_WORKSPACE_ROOT` would resolve against that cwd.

**The `Site::Remote` arm of `lifecycle::Ctx`** — the flush / `.ws-hooks/` push /
`PATH` harvest preamble. Anything added there reaches every op at once.

**File transport** — `mutagen` syncs the worktrees, `mutagen forward` maps server
ports back to `localhost`, and `remote::push_ws_hooks` is a *third* transport for
the workspace-root `.ws-hooks/` directory, which lives outside the repos and so
never rides the worktree sync.

Two smaller ones: `ports::port_free_at` binds a local `TcpListener` but asks the
server's `ss` remotely, requiring a literal `free` token back so a missing `ss`
reads as *busy* rather than silently handing out a taken port; and `caddy`'s
admin API is always dialled at `127.0.0.1`, because `curl` itself runs at the
site.

Everything else — routing, derivation, allocation, supervision, teardown — is one
code path with a `&Site` threaded through. `ws exec` is the one deliberate
exception: it injects with `Site::Local` even for a remote workspace, because its
`Inspect` phase neither allocates nor derives, and building a real `Site` would
mean dialling the server for a pre-check `ws exec` deliberately doesn't do.

---

## State on disk

| Path | Written by | Deleting it |
|---|---|---|
| `.ws.json` | you, `ws init`, `ws remote set` | is deleting the workspace config |
| `.ws.schema.json` | `ws init [--refresh-schema]` | fine — editor completion only |
| `.worktrees/<task>/<repo>/` | `ws new` (a real `git worktree`) | strands a git registration; use `ws rm` |
| `.ws-meta/<task>-<repo>.json` | `ports::resolve`, `record_services` | frees the ports; they'll be reallocated differently |
| `.ws-meta/<task>-<repo>.override.yml` | `compose::write_override` | fine — rewritten on every `up` |
| `.ws-meta/.lock` | `ports::Lock` | fine — it's stolen after 5s anyway |
| `~/.ws-run/<name>.{pid,log}` *at the site* | `supervise::up` | loses track of a live process |
| `~/.config/ws/` | `ws config set`, `ws setup --port` | resets machine-global defaults |

Offloaded workspaces add, on the server: `<workspace_path>/<task>/<repo>` (the
mutagen beta), `<workspace_path>/.ws-meta/*.override.yml`, and
`<workspace_path>/.ws-hooks/`. Caddy keeps its internal CA and its route autosave
in the `ws-caddy-data` and `ws-caddy-config` volumes — dropping those means every
client re-trusts a new root.

---

## Where not to look

- **`ops/up.rs` and `ops/down.rs`** look like the implementation of `ws up` and
  `ws down`. They are 24 lines of summary formatting each. The body is
  `ops/hooksweep.rs`, a name that appears nowhere in the CLI, the docs or the
  config schema.
- **`ops/mod.rs`** is not a layer. It is `ExitError` plus a handful of small
  shared helpers. It used to hold SSH plumbing; that moved to `lifecycle.rs`.
- **`remote.rs` below `Site`** is ~800 lines of SSH detail (multiplexing options,
  the login-`PATH` probe, `env`-word rendering, the `.ws-hooks/` tar push). You
  almost never need it. `Site` is the part that matters.
- **`specs/`** is gitignored and pre-implementation. A clone doesn't have it. Five
  comments in committed source cite `spec-*.md` files (`ui.rs`, `hooks.rs`,
  `ops/list.rs`, `mutagen.rs`) — those pointers are dead for anyone but the
  author. The code and `docs/` are authoritative.
- **`src/skills.rs` and `skills/`** are the `ws claude` payload, not part of any
  op's execution path.

### Deviations that look like bugs and aren't

Each of these has cost someone real time. Don't "fix" one without reading the
comment above it.

- **`inject::substitute` is hand-rolled and brace-only.** It replaces `${WS_…}`
  and *only* when the name is one we own. Not because nobody knew about template
  crates — the `subst` crate substitutes every `$VAR`, which would mangle exactly
  the files channel 3 exists to render. An nginx `$host` and an unknown
  `${WS_typo}` must both survive byte-for-byte.
- **Shell snippets are built as Rust strings** in `supervise.rs`,
  `ports::port_free_at`, `caddy`'s curl calls and `remote::remote_script`. Not
  laziness: the same string has to run through `sh -c` here *and* through the
  server's login shell over SSH. That is also why `supervise::owner_fn` re-splits
  pid lists through a command substitution — zsh doesn't word-split unquoted
  expansions, and `for t in $2` saw a whole pid tree as one word.
- **`remote::shell_quote` is a security invariant.** So are
  `compose::yaml_key`/`yaml_dquote`, `config::runtime_looks_safe`,
  `remote::connection_looks_safe` and `inject::ensure_within_worktree`. All of
  them exist because `.ws.json` is not gitignored and therefore arrives with a
  clone: an unquoted YAML key can reopen the generated override and grant every
  service `privileged: true`; a `runtime` of `.ws-hooks/helper` executes out of
  the clone on the first `ws setup`; a `connection` of `-oProxyCommand=…` makes
  `ssh` run a local command. The guards live at the boundary each value crosses,
  which is why they look scattered.
- **`workspace::worktrees` (git) and `all_task_dirs`/`orphan_dirs` (filesystem)
  both exist.** Git is the single definition of "a worktree exists" — it sees
  worktrees outside `.worktrees/`, knows the real branch, and knows a stale
  registration. The filesystem walk exists *precisely* to find what git can't see,
  so `ws clean` can report it. Neither is redundant.
- **`ws_vars` never emits `WS_PORT_*` straight off the `.ws-meta` map.**
  `ports::resolve` only ever adds keys, so a removed or now-HTTP service keeps a
  stale allocation forever. The vars are keyed off the *current* service set.
- **`caddy` never `POST /load`.** Add is a scoped PUT at index 0, delete is by
  `@id`. `/load` replaces the whole config and would clobber every other
  worktree's routes.
- **`hooks::refuse_legacy_hook_file`** is a migration shim, marked for deletion a
  release after 0.6, together with its tests. It can never produce a command.

---

## Working on it

```sh
cargo build
cargo test           # 381 tests, ~20s warm
cargo clippy --all-targets -- -D warnings
```

Tests are mostly **out-of-process**: they build the `ws` binary and run it against
a real temporary workspace with real git repos, real worktrees and real detached
processes. `tests/common/mod.rs` is the fixture and is worth reading before you
write your first test:

- `WorkspaceFixture::with_repos(&["api", "app"])` — a tempdir with `git init`ed
  repos and a matching `.ws.json`.
- `add_hook(repo, event, body)` writes an executable `.ws-hooks/<repo>-<event>.sh`
  and points a `scripts` entry at it; `add_script` declares one inline (use it when
  the test cares that `ws` is the shell's direct parent).
- `stub(name, body)` and `arg_recorder(name)` are canned executables to point
  `WS_SSH` / `WS_MUTAGEN` at — `arg_recorder` appends every argv to `$ARGS_FILE`,
  so you can assert the exact command `ws` would have run without a server.
- `run`/`run_ok`/`run_in` execute the binary with `HOME` redirected into the
  fixture, so `~/.config/ws` and `~/.ws-run` never touch the real ones.

The seams that make this work without a server, a container or a Caddy:

| Env var | Substitutes |
|---|---|
| `WS_SSH` | the `ssh` binary |
| `WS_MUTAGEN` | the `mutagen` binary |
| `WS_CADDY_ADMIN` | the Caddy admin URL — **and** puts `ws` in externally-managed mode, so it creates no network and runs no container. The fixture points it at an always-200 stub; a test that wants the failure path points it at a dead endpoint |
| `WS_HTTPS_PORT` | the port Caddy serves on, for one invocation |
| `HOME`, `CLAUDE_SKILLS_DIR` | the global config dir and the `ws claude` target |

`git` is deliberately **not** overridable — it always runs against real
repositories. A handful of tests are gated on an external binary
(`docker compose`, `ss`/`lsof`, `python3`) and print `skipping: …` rather than
failing when it isn't there.

**The convention that matters:** a fix ships with a regression test, and you
verify that the test *fails without the fix*. Stash the fix, run the test, watch
it go red, restore. A test written after the fact that passes on both sides of
the change proves nothing — and several of the bugs described in the comments
above (a `status` script that resolved without anchoring, a dashed service name
that died as a command over SSH, a multi-pid host process that every port read
back as `other`) were invisible for exactly that reason: the obvious assertion
held either way. Say what you did in the commit body — see
[CLAUDE.md](CLAUDE.md) for the message format.
