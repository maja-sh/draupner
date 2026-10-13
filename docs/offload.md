# Offloading a workspace to a server

You edit on your machine; the containers, the dev servers, and the routing all
run somewhere else. Useful when the stacks are heavier than your laptop enjoys,
or when you want several tasks running at once without the fans coming on.

The shape of it:

- Your clones and worktrees stay local, and your editor works on local files.
- `mutagen` keeps each worktree mirrored to the server, two-way and continuously.
- Every lifecycle command — `up`, `down`, `exec`, `status` — dispatches over SSH
  and runs *there*, beside the stack.
- Raw-TCP services are forwarded back to `localhost` on your machine, so a
  database client still connects to `localhost:20001`.
- HTTP services are reached by hostname through the server's Caddy.

## Granularity

**Offload is a property of a workspace root** — not of `wsxx`, and not of an
individual repo. Every repo in a workspace goes to the same place.

If you want an API on a server and a frontend on your laptop, that's two
workspace roots, each with its own `.ws.json`. (Cross-root service references
and shared routing are not solved today.)

---

## Setting it up

### 1. Prepare the server

The server needs `sshd`, a container runtime, `mutagen`, and `ss`, plus a couple
of pieces of configuration that genuinely need root. Copy
[`server-setup/debian.sh`](../server-setup/) to the box, read it, run it. It's
idempotent and it explains each step as it goes.

Make sure you can `ssh` to it non-interactively first — `wsxx` drives everything
over that connection, and an SSH host alias in `~/.ssh/config` is the tidiest way
to name it.

The server runs **podman** — that's what the script installs, and what a
workspace with a `remote` block and no `runtime` uses. Your own machine can run
docker regardless; `runtime` describes where the stacks run, not where you type
`wsxx`. See [configuration.md](configuration.md#runtime-is-per-site-not-per-machine).

### 2. Point the workspace at it

For a new workspace:

```sh
wsxx init --remote my-server --domain myapp.test
```

For one that already exists:

```sh
wsxx remote set my-server
```

Either writes a `remote` block to `.ws.json` and verifies the host is reachable
first, refusing rather than writing a config that can't work (`--no-verify` to
skip). `wsxx remote set` also backfills sync sessions for every worktree you
already have.

If you always use the same server, record it once:

```sh
wsxx config set remote.host my-server
wsxx remote set              # picks it up
```

Precedence is: the argument you passed > `$WS_REMOTE_HOST` > the global config.

### 3. Check it

```sh
wsxx remote check
```

Read-only. It probes SSH reachability and `mutagen` locally, then in a single
round trip on the server: `mutagen`, the container runtime, whether the routing Caddy
is running, whether the workspace directory exists and is writable, and — when
the workspace has a `domain` — whether the server resolves `*.<domain>` to
itself. `-v` showsx the exact command behind each probe.

### Coming back

```sh
wsxx remote unset
```

Removes the `remote` block and terminates the sync sessions. The worktrees stay
where they are.

Both directions of the flip change what `*.<domain>` should resolve to, and
neither one touches your resolver on its own — that stays a consented step. So
each **offers** to re-point it right there, with the same `[R]un / [S]kip /
show-me` prompt `wsxx setup` uses and the same commands; you see them before
anything runs, and `sudo` still prompts for itself. Skip it, or run the flip from
a script (no terminal to prompt at), and you get the old and new answers plus a
pointer to `wsxx setup` instead — re-run that, or the wildcard keeps aiming at the
machine you just stopped using.

---

## Where things live on the server

Worktrees land under a directory named after your workspace (so two projects on
one server don't collide), resolved against the SSH login home. Override it with
`--path` on `set`, or `workspace_path` in the config, and use an absolute path if
you want it outside the home directory.

Shared scripts in `.ws-hooks/` are pushed to the server on each `up`, which is
what lets a `scripts` entry like `".ws-hooks/api-up.sh"` resolve in both places.

`wsxx` also keeps one multiplexed SSH connection per server rather than
handshaking for every small command.

---

## Reaching it

Two independent questions: can the *browsxer* resolve the hostname, and can it
trust the certificate.

`wsxx` routes HTTP services through a Caddy container on the server, matching on
the Host header, so all branches share one HTTPS port (8443 by default; `wsxx setup
--port 443` if you want URLs without a suffix and are willing to grant the
privilege). For that to work from your machine:

- `*.<domain>` has to resolve to the server. A wildcard DNS entry, or the
  dnsmasq setup `server-setup/debian.sh` installs and wires.
- The CA that signs those certificates has to be trusted by your browsxer. `wsx
  setup` on your own machine reports whether it is.

### Two resolvers, not one

There are two separate "resolve `*.<domain>`" requirements, and they fail in
completely different ways. Keeping them apart is worth the paragraph:

**The client must resolve `*.<domain>` to the server's address.** Everyone who
routes anything needs this — it's how your browsxer finds a worktree at all.
`wsxx setup` on your own machine reports on it and walks you through fixing it.

The *correct* answer here is derived from where the stacks live: loopback for a
local workspace, the server's address for an offloaded one. Flipping between them
therefore invalidates it, which is why both flips offer to re-point it (and say
what's stale when you decline). `wsxx setup` and `wsxx up` report what the wildcard
actually resolves to when it disagrees — a wildcard pointing at some other
machine reads as `resolves to <address>, expected <address>`, not as "doesn't
resolve yet". They are different problems: one is a DNS entry you never made,
the other is one you made for a different workspace shape, and it will happily
send requests to a host you thought you'd stopped using.

**The server must resolve `*.<domain>` to itself.** Only needed when something
*running on the server* dials a routed hostname. In practice that means a
`kind: host` repo — a dev server running directly on the box, proxying `/api` to
a routed container. A pure container stack never needs it: containers reach each
other by compose service name, and Caddy reaches them by container name on
`ws-net`, so the server is never a client of its own routing. Step 2b of
`server-setup/debian.sh` wires it.

The server-side half is the one that breaks quietly. `/etc/resolv.conf` belongs
to whatever configures the interface, so a DHCP lease renewal can drop the
loopback nameserver hours or days after setup: dnsmasq stays up and answers
correctly when queried directly, but nothing queries it, and the host-side dev
server starts failing every proxy hop with `getaddrinfo ENOTFOUND`. The setup
script now defends against that (step 2c), and `wsxx remote check` probes it, so
it reads as one red line instead of an hour of confusion.

By default the server's Caddy publishes on all interfaces, because the entire
point is that another machine reaches it. On a box with a public interface, pin
it:

```json
"remote": { "connection": "my-server", "publish_address": "100.x.y.z" }
```

Any address your client can route to works — a tailnet address, a LAN address.
Nothing here assumes Tailscale. It's only read when the Caddy container is
created, so changing it recreates the container on the next `wsxx up` (routes
survive; they reload from the config volume).

Raw-TCP services don't need any of this: `wsxx up` creates a `mutagen forward` per
allocated port and prints the `localhost:<port>` dial string for each one. The
same port number on both ends, so nothing downstream has to know it moved.

---

## What runs where

| | Runs |
|---|---|
| `git`, worktree creation | client |
| `prepare` | **client**, before the sync — stage local-only data here |
| `post-create`, `up`, `down`, `post-remove`, `status` | server |
| `wsxx exec` | server (`--local` to force otherwise) |
| Port allocation bookkeeping | client (`.ws-meta` is the record) |
| Port *freeness* probe | server — the port must be free where it's published |

Your scripts see `WS_REMOTE=1` and `WS_CONNECTION`, and their `WS_WORKSPACE_ROOT`
/ `WS_WORKTREE` are the server's paths. See [hooks.md](hooks.md).

---

## Things worth knowing

**A flip is refused while a stack is running.** `wsxx down` followsx the config, not
the containers: after `wsxx remote set` a stack still running locally is beyond its
reach, and after `wsxx remote unset` so are the server's containers and routes. So
both commands check first — host processes via their supervision record, compose
projects via `compose ps` — and refuse, naming the branches that are up and the
`wsxx down <branch>` that clears them. The working order is stop, flip, start again.

If liveness can't be determined — `wsxx remote unset` on a workspace whose server
has gone away, say — that is also a refusal, not a shrug. Assuming "nothing is
running" is how a stack ends up somewhere nothing will look for it. `--force`
overrides either refusal, and prints the recovery sequence (flip back, `wsxx down`,
flip again) for whatever it left behind.

**`wsxx remote set/unset` rewrites `.ws.json`** through its own types, so any keys
it doesn't model are dropped. Everything documented in the schema round-trips.

**`ss` is required on the server.** Without it every port reads as busy and
allocation fails loudly rather than handing out a port something already holds.

**Dependency directories are decided by git, not disk.** `vendor/` and
`node_modules/` are kept off the sync when they're untracked — regardless of
whether they happen to exist locally yet — because the sync is two-way and a
server-side `composer install` would otherwise drag hundreds of megabytes back.
A *committed* `vendor/` is source, and does sync.

**A linked worktree's `.git` file is not synced.** It's a pointer into your local
filesystem, which doesn't exist on the server; git aborts on any command in a
worktree that has one. Server-side scripts therefore run without git metadata.
