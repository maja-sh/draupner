# server-setup — the privileged, one-time host bits

`ws` does the **rootless** routing setup itself (creates the `ws-net` network,
runs the Caddy container) — no prompts, no privilege. Three bits genuinely need
root, so `ws` never runs them silently. How they're delivered depends on the
host:

- **macOS host** (workspace owner's machine): `ws setup` walks each gap
  interactively — shows the exact command, `[R]un / [S]kip / show-me`. `[R]un`
  shells the command with its own `sudo`, so `sudo` prompts you directly; `ws`
  never sees your password.
- **Linux host / remote offload box**: the bits live in a versioned, readable
  script here that you run yourself. The script *is* the transparency — read it,
  then run it. `ws setup` on Linux reports what's missing and points you here.

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
server-setup/debian.sh myapp.test            # CA trust + wildcard DNS + linger
server-setup/debian.sh myapp.test --port 443 # …plus allow rootless :443
```

Run it as your normal user; it `sudo`s per privileged command. Every step is
idempotent — safe to re-run after upgrades.

## Adding a distro

Each `server-setup/<distro>.sh` is standalone and covers the same four sections
(CA trust, wildcard DNS, opt-in `:443`, linger). Copy `debian.sh` and swap the
distro-specific pieces:

- package install (`apt-get` → `dnf`/`pacman`/…),
- the CA trust-store path + refresh command (`update-ca-certificates` →
  `update-ca-trust`/`trust anchor`),
- the resolver wiring (systemd-resolved vs plain `resolv.conf` vs NetworkManager).

Keep it readable and idempotent — someone is going to read the whole thing
before they run it as root.
