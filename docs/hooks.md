# Lifecycle scripts and the environment contract

Everything project-specific happens in a lifecycle script. `drup` handles
worktrees, ports, hostnames and routing; your scripts do the part only your
project knows about — installing dependencies, seeding a database, starting a dev
server.

Declare them in `.drup.json` under a repo's `scripts`:

```json
"scripts": {
  "post-create": "composer install",
  "up": ".drup-hooks/api-up.sh",
  "down": "podman compose down"
}
```

Each value is one shell command or a list run in order, executed with `sh -c`
from the **worktree directory**. A value beginning `.drup-hooks/` is anchored to
the workspace root so it resolves wherever it runs; any other relative path is
not resolved for you.

---

## The events

| Event | When | Runs where |
|---|---|---|
| `prepare` | just before `up` | **always on the client** |
| `post-create` | after `drup new` creates the worktrees | at the site |
| `up` | `drup up` | at the site |
| `down` | `drup down` | at the site |
| `post-remove` | during `drup rm`, before the worktree goes | at the site |
| `status` | `drup list` | at the site |

"At the site" means: locally for a local workspace, and **on the server** for an
offloaded one. That's the whole point of the abstraction — the same script runs
in the same place relative to the stack, wherever the stack lives.

### `prepare` is the exception

`prepare` always runs on the client, even when the workspace is offloaded, and it
runs *before* the sync that carries the worktree to the server. That makes it the
place to stage anything that only exists on your machine: a dump of your main
checkout's database, a local dev CA, credentials that aren't in git. Whatever it
writes into the worktree rides the sync.

Because it sees `DRUP_REMOTE`, one script can branch — dump-for-sync when
offloaded, a same-host copy when not.

### `status` is special

It's read-only, capped at five seconds, and only its **first non-empty line** is
captured, for the `drup list` status column. A slower script is reported as
unknown.

Container repos get a default (`compose ps -q | wc -l`) if they declare nothing.
`kind: host` repos don't need one at all: `drup` supervises the process itself and
reports whether it's running, crashed, or running without serving the port it was
allocated. A declared `status` script still gets to speak when the process is
healthy — but when `drup` has bad news about the process, that wins.

### Defaults for container repos

A `kind: container` repo that declares no script for an event falls back to:

| Event | Default |
|---|---|
| `up` | `<runtime> compose up -d` |
| `down` | `<runtime> compose down` |
| `status` | `<runtime> compose ps -q \| wc -l` |
| `post-remove` | `<runtime> compose down -v` |

So a plain compose project frequently needs no `scripts` block at all.

---

## The environment

### Always present

| Variable | Value |
|---|---|
| `DRUP_WORKSPACE_ROOT` | The workspace root **at the execution site** |
| `DRUP_WORKTREE` | This repo's worktree path at the execution site |
| `DRUP_REPO` | The repo name |
| `DRUP_BRANCH_SANITIZED` | The branch as one path/DNS-safe label (`feat/x` → `feat-x`) |
| `DRUP_HOOK_NAME` | The event being run |
| `DRUP_RUNTIME` | The configured container runtime (`podman` / `docker`) |
| `DRUP_REMOTE` | `1` when the workspace is offloaded, else `0` |
| `DRUP_CONNECTION` | The SSH host when offloaded, else empty |

"At the execution site" matters: for an offloaded workspace these are the
*server's* paths, because that's where the script runs. A script referencing
`$DRUP_WORKSPACE_ROOT/.drup-hooks/…` therefore resolves in both worlds.

`DRUP_RUNTIME` exists so a shared script can call `$DRUP_RUNTIME compose …` instead
of hardcoding a runtime. `DRUP_CONNECTION` exists so a client-side `prepare` can
reach the server without a hostname duplicated out of `.drup.json`.

### Per-service, when the repo declares or derives services

| Variable | Value |
|---|---|
| `DRUP_HOST_<service>` | The routed hostname, `<branch>.<subdomain>.<domain>` (HTTP services, needs a `domain`) |
| `DRUP_PORT_<service>` | The allocated host port (raw-TCP services; for a `kind: host` repo, every service) |

Plus every mapping you declared in the repo's `env` block, with `${DRUP_*}` tokens
substituted. See [configuration.md](configuration.md#getting-values-into-your-app).

A `kind: host` repo gets a `DRUP_PORT_*` for *every* service including HTTP ones,
because the process has to bind something — the routed hostname then points at
it. Bind exactly the port you were given:

```json
"scripts": { "up": "exec vite --port \"$DRUP_PORT_web\"" }
```

### `drup exec`

Commands run through `drup exec` receive `DRUP_REPO`, `DRUP_BRANCH_SANITIZED`, and the
same injected service variables — identical on the local and remote paths, so a
command doesn't need to know where it landed.

---

## Interactive hooks (sudo prompts, confirmations)

A hook that needs human input mid-run — a `sudo` password, a `read -p` yes/no,
an MFA code paste — can opt into **interactive mode**. drup runs it with
inherited stdio so the prompt reaches the terminal, and refuses to run it at
all when there's no human to prompt (Claude Code / CI / `nohup`), so you don't
get a mysteriously-stuck hook with its password prompt swallowed by the
output pipeline.

Declare it in the object form of a `scripts` entry:

```json
"scripts": {
  "up": { "cmd": "sudo gcloud auth login", "interactive": true }
}
```

The object form takes a single `cmd` (chain with `&&` for multi-step) and the
`interactive` flag. Omitting `interactive` is the same as writing `false`,
same as the plain string / list forms.

### Who can prompt, who can't

| Invoker | Controlling terminal? | Behavior |
|---|---|---|
| You in a shell | yes | hook runs with inherited stdio, sudo prompts, you type |
| Claude Code / CI / `nohup` | no | drup refuses the hook, exits **3**, names what to re-run from your shell |

Exit **3** is distinct from the generic hook-failure exit **1** so an
automation wrapper (Claude's shell, a CI job) can tell "needs human attention"
apart from "actually broken" and surface the right next step.

Mixed cases — some repos refuse, others crash outright — exit **1**, because
a Claude wrapper watching for 3-vs-1 would otherwise retry assuming nothing
else is broken.

### What drup uses to detect

Opens `/dev/tty` with `O_RDWR`. Matches sudo's own test, catches both "no tty"
and "backgrounded" cases that `isatty(0)` would miss (e.g. `drup up < some-file`
redirects stdin but leaves /dev/tty available).

### Overriding from the shell

`DRUP_NON_INTERACTIVE=1` forces the refusal path even when a terminal is
available — useful for CI that happens to run under a pty, scripted
automation, or debugging the refusal UX from a real shell.

### Branching inside a hook

drup exports `DRUP_INTERACTIVE=1` to every hook iff the current invocation can
prompt. A hook that wants to handle non-interactive invocation itself
(custom message, custom exit) can branch:

```bash
#!/usr/bin/env bash
if [ -z "${DRUP_INTERACTIVE:-}" ]; then
  echo "needs interactive terminal; run from your shell" >&2
  exit 1
fi
sudo gcloud auth login
```

Not required — drup's own refusal (exit 3 with the message) is enough — but
the escape hatch is there if you want to customize.

### What's not supported (yet)

- **Interactive on `status`** — refused at `.drup.json` load. `status` fans
  out in parallel from `drup list`, and an interactive status probe would
  collide with every other repo's probe for the one terminal.
- **Interactive hooks on a remote workspace** — refused at load. There's no
  pty allocated over SSH, so the prompt can't actually reach you.

---

## Host processes

A `kind: host` repo's `up` script is started **detached**: `drup` launches it, notes
its PID and start time, and returns rather than blocking. So the script should
run the server in the foreground — `exec` into it where you can, so the process
`drup` tracks is the server itself.

- Output goes to `~/.drup-run/<name>.log` at the site, PID to `~/.drup-run/<name>.pid`.
- A process that dies immediately fails the `up` loudly, with the tail of its log.
- After launch, `drup` checks that something is listening on the port it allocated
  and that it belongs to the process it started. A different process holding that
  port is reported — that's the case where a dev server quietly moves to another
  port and everything downstream keeps dialling the old one.
- `drup down` stops the whole process tree, not just the recorded PID, so a wrapper
  shell can't leave the real server orphaned.
- There's no crash-restart. A process that dies stays dead until the next `drup up`
  — but `drup list` says so.

On an offloaded workspace the process runs on the server, and `drup` harvests the
`PATH` from your login shell there, so version-manager toolchains (nvm, asdf)
are found. A `PATH` you set in the repo's `env` wins over the harvested one.

---

## The `.drup-hooks/` directory

Anything longer than a line belongs in a shell file rather than inline JSON.
`.drup-hooks/` at the workspace root is where those live — `drup init` scaffolds a
starter `.drup-hooks/<repo>-<event>.sh` per event, and `drup` pushes the whole
directory to the server on each `up`, so a shared script works the same in both
places. Point a `scripts` entry at one:

```json
"scripts": { "up": ".drup-hooks/api-up.sh" }
```

Nothing in `.drup-hooks/` runs unless a `scripts` entry names it. The scaffolded
files are inert until you declare one.

Scripts must be executable. `drup` restores the executable bit on the copy it
pushes to a server, but a client-side script (notably `prepare`) that an editor
rewrote to `644` will fail with a bare `Permission denied` — `chmod +x` it.

### Upgrading from hook files (pre-0.6)

`drup` used to *discover* an executable `.drup-hooks/<repo>/<event>` and run it with
no `scripts` entry at all. That mechanism is gone; a leftover file now fails the
repo with the line to paste rather than silently doing nothing:

```json
"scripts": { "up": ".drup-hooks/api/up" }
```

The file itself keeps working, but its contract changed with the move — it now
runs with **cwd = the worktree** instead of the workspace root, and the worktree
path arrives as **`$DRUP_WORKTREE`** instead of `$1`. So `grep -n '\$1' .drup-hooks/`
and fix those, and prefix a `cd "$DRUP_WORKSPACE_ROOT"` anywhere the script relied
on running from the root. In exchange the hook becomes remote-capable: a declared
script runs on an offloaded workspace, which a discovered file never could.
