# Configuring a workspace

`.ws.json` sits at the workspace root and describes your local multi-repo layout.

**Every field is documented in `.ws.schema.json`**, which is generated from the
code and therefore cannot drift. Point your editor at it (`ws init` writes a
`$schema` reference for you) and you get completion, types, and per-field
descriptions for free. Regenerate it after upgrading `ws`:

```sh
ws init --refresh-schema
```

This page covers what a schema can't: how the pieces interact, what `ws` infers
on your behalf, and which rule wins when two say different things.

`.ws.json` is **per-machine**, not shared project config. It describes where
*your* clones live and where *your* stacks run, which is why the offload target
lives in it directly rather than in a separate overlay.

---

## Container runtime

`ws` drives **docker** or **podman** — for `compose config` derivation, the
shared `ws-net` network, and the routing Caddy container.

Leave `runtime` out and `ws` detects one on PATH, **docker first**. That's why
`ws init` doesn't write the key: an omitted `runtime` re-detects on whatever
machine opens the workspace, while a pinned one would freeze the answer from the
box that ran `init`.

Pin it when the detected choice is wrong for you — both installed and you want
podman, or your binary is under another name:

```sh
ws init --runtime podman     # or: add "runtime": "podman" to .ws.json
```

With neither installed, the operations that need one (`ws new` into a routed
workspace, `ws up`, `ws setup`) say so directly instead of failing on a missing
binary.

### `runtime` is per *site*, not per machine

For a workspace with a `remote` block, everything that spawns the runtime —
`compose config`, the `ws-net` network, the Caddy container — spawns it **on the
server**, over SSH. So `runtime` describes wherever this workspace's stacks
actually run, and the machine you type `ws` on doesn't come into it.

That makes the supported topology **docker or podman on your machine, podman on
the server** ([server-setup](../server-setup/) provisions podman: rootless, no
daemon, no root-equivalent group to join). A `remote` block with no `runtime`
therefore resolves to podman rather than detecting, because the client's PATH
says nothing about the server's.

The practical consequence: **leave `runtime` out** and a workspace gets docker
locally and podman when you offload it, with no edit in between. Pin it only to
override one of those.

One rough edge: the compose isolation lint on `ws new` is a static check over
the committed files, so it always runs locally. On a docker-only machine with a
podman-pinned workspace it yields nothing rather than erroring — you lose a
lint, not a command.

---

## Repos

A repo entry is either a bare name or an object:

```json
{
  "repos": [
    "docs",
    { "name": "api", "kind": "container" }
  ]
}
```

A bare name is convention-only — it gets worktrees and nothing else: no service
derivation, no port allocation, no injected environment. Anything beyond
"make me a worktree" needs the object form.

### Container or host

`kind` decides how a repo's stack is launched:

- **`container`** — a compose project. `ws` generates a per-worktree override
  that namespaces the project and reissues published ports.
- **`host`** — a long-running process that `ws` starts detached and tracks by PID
  file (see [hooks.md](hooks.md) for how it's launched, and `ws list` for how it
  reports).

Omit `kind` and `ws` infers `container` when a compose file is present. The
inference is deliberately one-way: it will never infer `host`, because
supervision changes `up` from a blocking script into a detached process, and
that shouldn't happen to a repo by accident. **A host repo must say so.**

---

## Services and routing

A service is a thing that listens. `ws` classifies each one as HTTP or raw TCP,
and that classification decides how parallel worktrees are kept apart:

- **HTTP** services get a *hostname*: `<branch>.<subdomain>.<domain>`, routed by
  a shared Caddy. Many branches share one HTTPS port because the Host header
  tells them apart. Exposed to your scripts as `WS_HOST_<service>`.
- **raw TCP** services get an allocated *host port* — there's no host header to
  route on, so each one needs its own. Exposed as `WS_PORT_<service>`.

Routing only happens when a top-level `domain` is set. Without one, HTTP services
simply aren't routed and you reach things on ports.

```json
{
  "domain": "myapp.test",
  "repos": [{
    "name": "api",
    "kind": "container",
    "services": {
      "web": { "http": "api", "port": 80 },
      "db":  { "port": 3306 }
    }
  }]
}
```

`"http"` takes a string (the subdomain label), `true` (use the service name), or
is absent/`false` (raw TCP). Above, `web` becomes
`feat-checkout.api.myapp.test` and `db` gets something like `:20001`.

Set `"wildcard": true` on an HTTP service to also route *every* subdomain of its
host, for a multi-tenant app addressed as `<tenant>.<branch>.<sub>.<domain>`.
`WS_HOST_<svc>` stays the base host, so the app builds tenant URLs by prefixing
it.

### Derivation: you usually don't write this block

For a container repo, `ws` runs `<runtime> compose config` at `ws up` and derives
the service set from the compose file itself. Your declared `services` are an
*overlay*: declared wins per key, derived fills in the rest.

What derivation does:

- One entry per published port. A service publishing a single port keeps its own
  name; one publishing several is split into `<service>_<port>` keys (so a
  mailpit publishing 1025 and 8025 yields `mailpit_1025` and `mailpit_8025`, and
  the underscore keeps `WS_PORT_*` shell-clean).
- **Only ports 80 and 443 are treated as HTTP.** Anything else is raw TCP.
  Conservative on purpose: a wrong guess here silently changes how a service is
  reached. If your app serves HTTP on 8000, declare it.
- `expose`-only ports (not published) are not derived — nothing to de-conflict.

If a repo is *explicitly* `kind: container` and the derivation fails, `ws` stops:
a broken compose file should surface, not quietly produce an unrouted stack. If
the kind was merely inferred, it falls back to your declared services with a
note.

The effective set is recorded when a stack comes up, so read-only commands
(`down`, `list`, `status`, remote `exec`) resolve the same hosts and ports
without re-running compose.

### Not yet implemented

`shared` (run a service once as shared infra instead of per-worktree) is accepted
by the schema and **currently does nothing** — every service is per-worktree.

---

## Getting values into your app

`ws` owns `WS_HOST_*` and `WS_PORT_*`. You decide which of *your* variables
receive them, through three channels — all of them format-blind, none of them
requiring `ws` to understand your config file.

### 1. `env` — the primary channel

Your app's variable name on the left, a `${WS_*}` template on the right:

```json
"env": {
  "APP_URL": "https://${WS_HOST_web}",
  "DB_PORT": "${WS_PORT_db}",
  "DOCKER_COMMAND": "${WS_RUNTIME}"
}
```

These are set in the environment of every lifecycle script, and for a container
repo they're written into the generated compose override as well.

Substitution is deliberately **brace-only** (`${WS_FOO}`, never `$WS_FOO`) and
touches nothing but `WS_`-prefixed names — so an nginx template full of `$host`
passes through untouched. An unresolved token is left literal rather than
blanked, which is what makes a missing value visible instead of silently empty.

### 2. Cross-repo references

A repo's `env` and `templates` can reference a *sibling* repo's service:

```json
"env": { "SSO_REDIRECT": "https://localhost:${WS_PORT_app.web}" }
```

`${WS_HOST_<repo>.<svc>}` and `${WS_PORT_<repo>.<svc>}` read the peer's recorded
allocation. Peers are read-only here — a peer that has never been brought up has
no allocation yet, so the token stays literal.

### 3. `templates` — the escape hatch

When a value has to land inside a config file that reads nothing from the
environment:

```json
"templates": [{ "src": "config/app.conf.tmpl", "dest": "config/app.conf" }]
```

`src` is committed, `dest` is rendered on every `up` and added to the worktree's
git excludes so it never shows up as a change. `ws` refuses to render over a
tracked file.

### `copy` vs `templates`

Both put files into a fresh worktree; they are not interchangeable.

- **`templates`** renders a committed source with substitution. Use it when the
  content depends on this worktree's ports or hostnames.
- **`copy`** seeds *untracked* files verbatim from your main checkout — dev
  certs, keys, a local `.env`. No substitution, no clobbering an existing file, a
  source that matches nothing is skipped. Entries are repo-relative paths, dirs
  (copied recursively), or globs.

```json
"copy": ["storage/oauth-*.key", ".env.local", "certs/"]
```

---

## Lifecycle commands

`scripts` maps an event name to one command or a list run in order:

```json
"scripts": {
  "up": ["composer install", "php artisan migrate"],
  "status": "podman compose ps -q | wc -l"
}
```

Each runs via `sh -c` with the worktree as the working directory. A container
repo that declares nothing gets sensible defaults (`compose up -d`,
`compose down`, `compose ps`, and `compose down -v` on removal), so a plain
compose project often needs no `scripts` at all.

A value beginning `.ws-hooks/` is anchored to the workspace root automatically,
so `".ws-hooks/api-up.sh"` resolves whether the script runs locally or on an
offload server. **Any other relative path is not resolved** — it runs against the
worktree's working directory.

See [hooks.md](hooks.md) for the events, where each one runs, and the full
environment contract.

---

## Two fields that get misread

**`worktree_excludes`** appends entries to a worktree's
`.git/info/exclude`. It is a *git visibility* control and nothing else — it hides
files from `git status`. It has no bearing on what is synced to an offload
server; files seeded by `copy` are excluded from git precisely so they *can* ride
the sync.

**`hooks`** is the legacy per-event file mechanism, superseded by `scripts`.
Legacy hook files run from the workspace root with the worktree path as `$1`,
which also means they only exist on the client — a remote workspace can't run
them, so `ws list` will skip a legacy `status` hook when offloaded. Prefer
`scripts`.

---

## What `ws` writes next to your config

- **`.worktrees/<branch>/<repo>/`** — the worktrees themselves.
- **`.ws-meta/`** — per-machine state: the port allocation for each worktree, the
  effective service set recorded at `up`, and the generated compose override.
  Disposable, but deleting it loses stable port assignments.

Both are `ws`'s to manage. Add them to the workspace's ignore rules if the
workspace directory is itself a git repo.

---

## A note on compose files

At `ws new`, `ws` lints each repo's compose file for two things that break
worktree isolation:

- a pinned `container_name`, which is global — a second worktree can't start.
- an `external:` volume, shared across worktrees, so one branch's migration lands
  in another branch's database.

Both are warnings, not refusals. `ws` never edits your compose file; the
per-worktree override it generates lives in `.ws-meta/`.
