# How a URL reaches a worktree

`https://feat-checkout.app.myapp.test:8443` in a browser, a running worktree at
the other end. This page walks the chain between them, then maps each way it
breaks to the link that broke. If you're diagnosing, skim the chain once and
jump to the [symptom table](#when-it-doesnt-work).

## The chain

1. **Wildcard DNS.** `*.myapp.test` must resolve to the machine running Caddy —
   loopback for a local workspace, the server's routable address for an
   offloaded one. Nothing registers individual hostnames; one wildcard covers
   every branch and service, which is why new worktrees need no DNS work.
   `ws setup` wires this and `ws remote check` verifies it.

2. **TLS.** Caddy signs a certificate per hostname on demand from its own
   internal CA. Your machine trusts that CA's root once (`ws setup` offers the
   command), and every worktree hostname is covered from then on. The root
   survives Caddy re-creation — it lives in the `ws-caddy-data` volume.

3. **Caddy.** One `ws-caddy` container per machine, listening on `:8443` by
   default (`:443` is opt-in via `ws setup --port`). It routes on the Host
   header. `ws up` installs one route per HTTP service; `ws rm`/`ws clean`
   remove them. No match falls through to the "stack is stopped" page — which
   is itself a diagnostic: seeing it proves DNS, TLS and Caddy all work, and
   only the route is missing.

4. **The upstream hop.** A container service is dialed as
   `<project>-<service>-1:<port>` across the shared `ws-net` network — the
   generated compose override joined the service to that network and dropped
   its host publish, so Caddy is the only way in. A `kind: host` process is
   dialed at `host.containers.internal:<allocated port>` instead: it lives on
   the host, keeps its real port, and stays directly reachable there too.

5. **The service itself**, listening on the container port its compose file
   declares (or the `services` block declares, for a host repo).

For an offloaded workspace the same chain runs on the server; the only client
side parts are DNS (the wildcard answers with the server's address) and the
CA trust.

## When it doesn't work

| Symptom | Broken link | Where to look |
|---|---|---|
| "Can't find server" / NXDOMAIN | 1 — wildcard DNS | `ws setup` (local) or `ws remote check` (offloaded) |
| Certificate warning | 2 — CA root not trusted on *this* machine | `ws setup`'s trust step; each client machine trusts separately |
| "Stack is stopped" page | 3 — no route | `ws up` didn't run for this branch, or its route install failed — a container repo whose route fails is reported failed, so re-read the `ws up` output |
| 502 from Caddy | 4 or 5 — route exists, upstream doesn't answer | `podman logs ws-caddy` names the upstream it dialed; then the service's own logs. For a host repo, `ws list` shows the process `Health` |
| Works on the server, not from your machine | 1/2 client-side | The wildcard resolves to the wrong address here, or this machine hasn't trusted the CA — `ws remote check` covers both |
| Every worktree suddenly unreachable | 3 — Caddy or its routes went away | `ws up` any branch re-creates Caddy and reinstalls its own routes; other live branches need their own `ws up` to reinstall theirs |

Raw-TCP services never enter this chain: they get an allocated host port
(`WS_PORT_<svc>`), published directly — locally on the machine, or forwarded
back to `localhost` by mutagen for an offloaded workspace.
