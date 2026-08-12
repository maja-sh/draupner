# server-setup — the one-time host bits

`ws` does the **rootless** routing setup itself (creates the `ws-net` network,
runs the Caddy container) — no prompts, no privilege. What's left is the stuff
`ws` needs but shouldn't do to your machine behind your back: the packages it
expects to find, and three bits that genuinely need root. How they're delivered
depends on the host:

- **macOS workspace owner's machine**: `ws setup` walks each gap
  interactively — shows the exact command, `[R]un / [S]kip / show-me`. `[R]un`
  shells the command with its own `sudo`, so `sudo` prompts you directly; `ws`
  never sees your password.
- **Linux offload server**: the bits live in a versioned, readable script here
  that you run on the server yourself. The script *is* the transparency — read
  it, then run it. `ws setup` and `ws remote check` will point at this script
  when they find something missing.

## Prerequisites

The binaries `ws` invokes over SSH. The script checks each one and installs
what's missing.

| Binary | Package | Needed for |
|---|---|---|
| `podman` | `podman` | running your container stacks |
| `docker-compose` | `docker-compose` | the Compose provider `podman compose` delegates to |
| `ss` | `iproute2` | allocating free host ports for raw-TCP services |
| `mutagen` | (release tarball) | receiving the worktree file sync |
| `curl` | `curl` | fetching mutagen |
| `sshd` | `openssh-server` | the transport for all of the above |

`timeout` and the rest of coreutils ship with Debian. `kind: host` repos need
nothing further: `ws` supervises those as its own detached, PID-tracked
processes rather than as service-manager units.

Two pieces of configuration matter as much as the packages, and the script
handles both:

- **The podman API socket.** `podman compose` hands the work to Docker's Compose
  binary, which speaks the Docker API over podman's socket. If it isn't enabled,
  every `ws up` fails with "Cannot connect to the Docker daemon".
- **Constraining dnsmasq.** Left alone, dnsmasq answers on *every* interface,
  including the gateway of podman's container network. `aardvark-dns` then can't
  bind port 53, so the `ws` network — and the Caddy container on it — won't start
  at all. The script pins dnsmasq to the addresses clients actually use.

### Debian 13 or newer

`ws` generates a compose override using the `!override` tag, so it needs
**podman ≥ 4.7** (earlier releases have no `compose` subcommand at all) and a
**Compose provider ≥ 2.24.4**. Debian 12 ships podman 4.3.1 and docker-compose
1.29.2 and satisfies neither, so container repos cannot work there — use Debian
13 (podman 5.4.2 + docker-compose 2.26.1) or install both from upstream. The
script checks this and says so rather than letting `ws up` fail later with a
YAML parse error.

## The three privileged bits

| Bit | Why | Needed for |
|---|---|---|
| Trust Caddy's internal **root CA** | browsers stop warning on the TLS cert | any browser use |
| **Wildcard DNS** `*.<domain>` → 127.0.0.1 | per-worktree hostnames resolve | any browser use |
| (opt-in) rootless bind to **`:443`** | drops the `:8443` suffix from URLs | clean URLs only |

`ws` serves TLS on the non-privileged **`:8443`** by default, so the first two
bits are enough for a working, warning-free setup. `:443` is a cosmetic opt-in
(`ws setup --port 443` + the sysctl below).

## Usage

Caddy must be up first (`ws setup`, or `ws up <branch>`) so its root CA exists.

```sh
server-setup/debian.sh myapp.test                      # workspaces live on this box
server-setup/debian.sh myapp.test --address 10.0.0.5   # offload box: what clients reach it at
server-setup/debian.sh myapp.test --port 443           # …plus allow rootless :443
```

Run it as your normal user; it `sudo`s per privileged command. Every step is
idempotent — safe to re-run after upgrades. It's also meant to be *read* — if
you'd rather not run a script on a machine, the steps are plain enough to paste
one at a time.

`--address` is what `*.<domain>` answers with. It defaults to `127.0.0.1`, which
is correct when `ws` runs on this same machine. An **offload** box needs the
address its clients actually reach it at, or clients will resolve the hostnames
to their own loopback. Any routable address works — LAN IP, tailnet IP, VPN
address. **Tailscale is one option, not a requirement**; all `ws` needs is SSH
access plus an address that routes.

## Adding a distro

Each `server-setup/<distro>.sh` is standalone and covers the same five sections
(prerequisites, CA trust, wildcard DNS, opt-in `:443`, linger). Copy `debian.sh`
and swap the distro-specific pieces:

- package install (`apt-get` → `dnf`/`pacman`/…),
- the CA trust-store path + refresh command (`update-ca-certificates` →
  `update-ca-trust`/`trust anchor`),
- the resolver wiring (systemd-resolved vs plain `resolv.conf` vs NetworkManager),
  including how that wiring is kept when the DHCP lease renews — whatever
  configures the interface rewrites `/etc/resolv.conf` on its own schedule.

Keep it readable and idempotent — someone is going to read the whole thing
before they run it as root.
