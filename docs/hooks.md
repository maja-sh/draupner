# Lifecycle scripts and the environment contract

Everything project-specific happens in a lifecycle script. `ws` handles
worktrees, ports, hostnames and routing; your scripts do the part only your
project knows about — installing dependencies, seeding a database, starting a dev
server.

Declare them in `.ws.json` under a repo's `scripts`:

```json
"scripts": {
  "post-create": "composer install",
  "up": ".ws-hooks/api-up.sh",
  "down": "podman compose down"
}
```

Each value is one shell command, or a list:

```json
"scripts": {
  "up": [
    "composer install",
    "php artisan migrate"
  ]
}
```

A list is joined with `&&` into a **single** `sh -c`, which is the reason to
prefer it over cramming commands into one string with `;`:

- **Fail-fast.** A step that exits non-zero stops the rest. `;` would carry on.
- **One shell, so state carries.** A `cd` or an env assignment in one entry still
  applies to the next.
- **Anchoring is per entry**, so you can mix a shared script into a list:
  `["npm ci", ".ws-hooks/app-seed.sh"]`.

Everything runs with the **worktree** as the working directory. A value beginning
`.ws-hooks/` is anchored to the workspace root so it resolves wherever it runs;
any other relative path is not resolved for you.

---

## The events

| Event | When | Runs where |
|---|---|---|
| `prepare` | just before `up` | **always on the client** |
| `post-create` | after `ws new` creates the worktrees | at the site |
| `up` | `ws up` | at the site |
| `down` | `ws down` | at the site |
| `post-remove` | during `ws rm`, before the worktree goes | at the site |
| `status` | `ws list` | at the site |

"At the site" means: locally for a local workspace, and **on the server** for an
offloaded one. That's the whole point of the abstraction — the same script runs
in the same place relative to the stack, wherever the stack lives.

### `prepare` is the exception

`prepare` always runs on the client, even when the workspace is offloaded, and it
runs *before* the sync that carries the worktree to the server. That makes it the
place to stage anything that only exists on your machine: a dump of your main
checkout's database, a local dev CA, credentials that aren't in git. Whatever it
writes into the worktree rides the sync.

Because it sees `WS_REMOTE`, one script can branch — dump-for-sync when
offloaded, a same-host copy when not.

### `status` is special

It's read-only, capped at five seconds, and only its **first non-empty line** is
captured, for the `ws list` status column. A slower script is reported as
unknown.

Container repos get a default (`compose ps -q | wc -l`) if they declare nothing.
`kind: host` repos don't need one at all: `ws` supervises the process itself and
reports whether it's running, crashed, or running without serving the port it was
allocated. A declared `status` script still gets to speak when the process is
healthy — but when `ws` has bad news about the process, that wins.

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
| `WS_WORKSPACE_ROOT` | The workspace root **at the execution site** |
| `WS_WORKTREE` | This repo's worktree path at the execution site |
| `WS_REPO` | The repo name |
| `WS_BRANCH_SANITIZED` | The branch as one path/DNS-safe label (`feat/x` → `feat-x`) |
| `WS_HOOK_NAME` | The event being run |
| `WS_RUNTIME` | The configured container runtime (`podman` / `docker`) |
| `WS_REMOTE` | `1` when the workspace is offloaded, else `0` |
| `WS_CONNECTION` | The SSH host when offloaded, else empty |

"At the execution site" matters: for an offloaded workspace these are the
*server's* paths, because that's where the script runs. A script referencing
`$WS_WORKSPACE_ROOT/.ws-hooks/…` therefore resolves in both worlds.

`WS_RUNTIME` exists so a shared script can call `$WS_RUNTIME compose …` instead
of hardcoding a runtime. `WS_CONNECTION` exists so a client-side `prepare` can
reach the server without a hostname duplicated out of `.ws.json`.

### Per-service, when the repo declares or derives services

| Variable | Value |
|---|---|
| `WS_HOST_<service>` | The routed hostname, `<branch>.<subdomain>.<domain>` (HTTP services, needs a `domain`) |
| `WS_PORT_<service>` | The allocated host port (raw-TCP services; for a `kind: host` repo, every service) |

Plus every mapping you declared in the repo's `env` block, with `${WS_*}` tokens
substituted. See [configuration.md](configuration.md#getting-values-into-your-app).

A `kind: host` repo gets a `WS_PORT_*` for *every* service including HTTP ones,
because the process has to bind something — the routed hostname then points at
it. Bind exactly the port you were given:

```json
"scripts": { "up": "exec vite --port \"$WS_PORT_web\"" }
```

### `ws exec`

Commands run through `ws exec` receive `WS_REPO`, `WS_BRANCH_SANITIZED`, and the
same injected service variables — identical on the local and remote paths, so a
command doesn't need to know where it landed.

---

## Host processes

A `kind: host` repo's `up` script is started **detached**: `ws` launches it, notes
its PID and start time, and returns rather than blocking. So the script should
run the server in the foreground — `exec` into it where you can, so the process
`ws` tracks is the server itself.

- Output goes to `~/.ws-run/<name>.log` at the site, PID to `~/.ws-run/<name>.pid`.
- A process that dies immediately fails the `up` loudly, with the tail of its log.
- After launch, `ws` checks that something is listening on the port it allocated
  and that it belongs to the process it started. A different process holding that
  port is reported — that's the case where a dev server quietly moves to another
  port and everything downstream keeps dialling the old one.
- `ws down` stops the whole process tree, not just the recorded PID, so a wrapper
  shell can't leave the real server orphaned.
- There's no crash-restart. A process that dies stays dead until the next `ws up`
  — but `ws list` says so.

On an offloaded workspace the process runs on the server, and `ws` harvests the
`PATH` from your login shell there, so version-manager toolchains (nvm, asdf)
are found. A `PATH` you set in the repo's `env` wins over the harvested one.

---

## The `.ws-hooks/` directory

Reach for a shell file when a step needs real logic — loops, conditionals,
several stages of setup. A handful of straightforward commands is what the list
form above is for; you don't need a file for two lines.

`.ws-hooks/` at the workspace root is where those files live. `ws` pushes the
whole directory to the server on each `up`, so a shared script works the same in
both places. Create them yourself and point a `scripts` entry at one:

```json
"scripts": { "up": ".ws-hooks/api-up.sh" }
```

Nothing in `.ws-hooks/` runs unless a `scripts` entry names it — the directory is
a place you keep scripts, not a place `ws` looks.

Scripts must be executable. `ws` restores the executable bit on the copy it
pushes to a server, but a client-side script (notably `prepare`) that an editor
rewrote to `644` will fail with a bare `Permission denied` — `chmod +x` it.

### Upgrading from hook files (pre-0.6)

`ws` used to *discover* an executable `.ws-hooks/<repo>/<event>` and run it with
no `scripts` entry at all. That mechanism is gone; a leftover file now fails the
repo with the line to paste rather than silently doing nothing:

```json
"scripts": { "up": ".ws-hooks/api/up" }
```

The file itself keeps working, but its contract changed with the move — it now
runs with **cwd = the worktree** instead of the workspace root, and the worktree
path arrives as **`$WS_WORKTREE`** instead of `$1`. So `grep -n '\$1' .ws-hooks/`
and fix those, and prefix a `cd "$WS_WORKSPACE_ROOT"` anywhere the script relied
on running from the root. In exchange the hook becomes remote-capable: a declared
script runs on an offloaded workspace, which a discovered file never could.
