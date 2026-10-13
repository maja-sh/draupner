#!/usr/bin/env bash
# ws host setup — Debian / Ubuntu
# ================================
#
# Everything `ws` needs on a host but deliberately does NOT do to it for you:
# the packages it expects to find, and the privileged one-time bits. `ws` handles
# the rootless pieces itself (the `ws-net` network, the Caddy container); the
# rest lives here — a readable script you run yourself. That's the transparency
# contract: nothing privileged happens invisibly.
#
# Podman only, deliberately. `ws` drives either podman or docker on the machine
# you run it *from*, but a server is podman: rootless by default, no daemon, and
# no root-equivalent group to join. Debian's docker packages are also too old for
# the Compose features `ws` generates. So a workspace that offloads here runs on
# podman — which is exactly what an absent `runtime` in `.ws.json` resolves to.
#
#   0. Prerequisites  → podman, a Compose provider, iproute2, mutagen, curl, sshd.
#   1. Trust Caddy's internal root CA  → browsers stop warning on the TLS cert.
#   2. Wildcard DNS  *.<domain> → <address>  → per-worktree hostnames resolve.
#   2b. Local resolver  → this box resolves them too, which a `kind: host` repo
#       needs to reach a routed container from a dev server running here.
#   2c. Defend that wiring  → a DHCP lease renewal otherwise rewrites it away.
#   3. (opt-in) Rootless :443  → drop the `:8443` suffix from URLs.
#   4. Linger  → `systemd --user` host-process units survive logout.
#   5. Podman API socket  → what `podman compose` talks to.
#
# Every step is idempotent — safe to re-run. Nothing here depends on Tailscale;
# any address this box is reachable at will do (see the DNS step).
#
# Usage:
#   server-setup/debian.sh <domain> [--address <ip>] [--port 443]
#   WS_DOMAIN=myapp.test server-setup/debian.sh
#
# `--address` is what `*.<domain>` resolves to; it defaults to 127.0.0.1, which
# is right when `ws` runs on this same box. On an offload box, pass the address
# its clients reach it at — any routable address works, Tailscale or not.
#
# Run it as your normal user; it invokes `sudo` per privileged command (so sudo
# prompts you directly — the script never reads your password). Caddy must be up
# first so its root CA exists — `wsx setup` or `wsx up` starts it, run on whichever
# machine `ws` lives on. On an **offload box** that is a *different* machine:
# `ws` is deliberately never installed on the server, so every `ws` command named
# in this script's output runs on the client and reaches this box over SSH.

set -euo pipefail

CADDY_CONTAINER="ws-caddy"
CA_DEST="/usr/local/share/ca-certificates/ws-caddy-root.crt"
SYSCTL_FILE="/etc/sysctl.d/99-ws-unprivileged-ports.conf"
# Bump when a newer mutagen is wanted; the client and server should match.
MUTAGEN_VERSION="0.18.1"
# Upstream publishes SHA256SUMS but nothing this script can chain trust to, so
# the digests are pinned here instead: a mutated release, a hijacked CDN or a
# TLS-intercepting proxy fails the check rather than getting installed as root.
# Bump these together with MUTAGEN_VERSION (they're in the release's SHA256SUMS).
MUTAGEN_SHA256_amd64="7735286c778cc438418209f24d03a64f3a0151c8065ef0fe079cfaf093af6f8f"
MUTAGEN_SHA256_arm64="bcba735aebf8cbc11da9b3742118a665599ac697fa06bc5751cac8dcd540db8a"
# `ws` generates a compose override that uses the `!override` tag, which Compose
# only understands from this version on.
COMPOSE_MIN="2.24.4"

domain="${WS_DOMAIN:-}"
# What `*.<domain>` should answer with. Loopback is right when the workspaces
# live on this same machine; an offload box wants the address its clients
# actually reach it at (LAN IP, tailnet IP, whatever routes).
address="${WS_ADDRESS:-127.0.0.1}"
want_443=0

# Whether this box is an *offload server* (it runs the stacks; `ws` itself runs
# on a client and drives it over SSH) rather than the machine running `ws`. The
# answer address is the signal: loopback only makes sense when the workspaces
# live here. It decides which commands the guidance below can honestly name —
# on an offload box there is no local `ws` to invoke, by design.

# No local-vs-offload predicate here on purpose. This script runs on the box that
# hosts the stacks, and whether `ws` also runs on it is the operator's business,
# not something to infer and act on. The previous version guessed from the DNS
# answer (`address != 127.0.0.1`) and got it wrong for a real setup — `ws` running
# locally on a box that also serves peers on the LAN takes an `--address` for
# their benefit, and was then told `ws` doesn't run here. Every hint below is
# phrased to hold either way; `wsx remote check` reports on the server when a
# `remote` block exists and on the local host otherwise, so it needs no branch.

while [ $# -gt 0 ]; do
  case "$1" in
    # Only treat `--port 443` / `--port=443` as the opt-in; consume the `443`
    # token when present, but never swallow a non-port argument (e.g. the domain).
    --port) case "${2:-}" in 443) want_443=1; shift ;; esac ;;
    --port=443) want_443=1 ;;
    --address)
      # Check before shifting: shifting past the end aborts under `set -e`, which
      # would exit silently instead of reaching the message below.
      [ $# -ge 2 ] || { echo "error: --address needs a value" >&2; exit 2; }
      address="$2"; shift ;;
    --address=*) address="${1#*=}" ;;
    # The header block up to the first blank line.
    -h|--help) sed -n '2,/^$/p' "$0"; exit 0 ;;
    # Without these two guards a typo silently becomes the domain: `--port 8443`
    # falls through (only 443 is consumed above) and would be written into the
    # dnsmasq config as if it were the routing domain.
    -*) echo "error: unknown option '$1'" >&2; exit 2 ;;
    *)
      [ -z "$domain" ] || { echo "error: unexpected argument '$1'" >&2; exit 2; }
      domain="$1" ;;
  esac
  shift
done

if [ -z "$domain" ]; then
  echo "error: pass the routing domain, e.g. \`$0 myapp.test\`" >&2
  exit 2
fi
case "$domain" in
  *[!a-zA-Z0-9.-]*) echo "error: refusing an unsafe domain '$domain'" >&2; exit 2 ;;
esac
# Both checks matter: the first keeps shell/path metacharacters out of the config
# line, the second catches a well-formed-looking non-address. dnsmasq refuses to
# start on a bad `address=` line, so an unchecked value takes the resolver down.
case "$address" in
  "") echo "error: --address needs a value" >&2; exit 2 ;;
  *[!a-zA-Z0-9.:]*) echo "error: refusing an unsafe address '$address'" >&2; exit 2 ;;
esac
if ! printf '%s' "$address" |
     grep -qE '^([0-9]{1,3}(\.[0-9]{1,3}){3}|[0-9a-fA-F]*:[0-9a-fA-F:]*)$'; then
  echo "error: '$address' is not an IPv4 or IPv6 address" >&2
  exit 2
fi

say()  { printf '\n\033[1m▸ %s\033[0m\n' "$*"; }
note() { printf '  %s\n' "$*"; }

# ---------------------------------------------------------------------------
# 0. Prerequisites — the binaries `ws` calls on this box
# ---------------------------------------------------------------------------
# `ws` drives this host over SSH; everything checked below is something it
# actually invokes. `timeout` and the rest of coreutils ship with Debian already.
# `kind: host` repos need nothing beyond this list — `ws` supervises those as its
# own detached, PID-tracked processes, not as service-manager units.

# Report whether $1 is present; return non-zero so the caller can collect it.
# /usr/sbin and /sbin hold the daemons but aren't on a normal user's PATH.
have_bin() {
  local bin="$1" why="$2"
  if PATH="$PATH:/usr/sbin:/sbin" command -v "$bin" >/dev/null 2>&1; then
    note "✓ $bin"
    return 0
  fi
  note "✗ $bin — $why"
  return 1
}

# Docker ships Compose v2 as a CLI plugin, which never lands on PATH — a
# name-only check would reinstall it on a box that already has a working one.
compose_provider_present() {
  if command -v docker-compose >/dev/null 2>&1; then return 0; fi
  if [ -x /usr/libexec/docker/cli-plugins/docker-compose ]; then return 0; fi
  if [ -x "${HOME:-/root}/.docker/cli-plugins/docker-compose" ]; then return 0; fi
  return 1
}

install_mutagen() {
  local arch sha tmp
  case "$(uname -m)" in
    x86_64)        arch=amd64; sha="$MUTAGEN_SHA256_amd64" ;;
    aarch64|arm64) arch=arm64; sha="$MUTAGEN_SHA256_arm64" ;;
    *)
      note "no pinned mutagen build for $(uname -m) — install it by hand:"
      note "  https://github.com/mutagen-io/mutagen/releases"
      return 0
      ;;
  esac
  local url="https://github.com/mutagen-io/mutagen/releases/download/v${MUTAGEN_VERSION}"
  url="$url/mutagen_linux_${arch}_v${MUTAGEN_VERSION}.tar.gz"
  note "installing mutagen v${MUTAGEN_VERSION} from"
  note "  $url"

  # Staged in a temp dir and only installed once verified. Extracting straight
  # into /usr/local/bin would leave a truncated binary behind on a failed
  # download — and every later run would skip it, since the name now exists.
  tmp="$(mktemp -d)"
  if ! curl -fsSL --proto '=https' --proto-redir '=https' -o "$tmp/m.tgz" "$url"; then
    note "✗ download failed — install mutagen by hand from the URL above."
    rm -rf "$tmp"; return 0
  fi
  if ! printf '%s  %s\n' "$sha" "$tmp/m.tgz" | sha256sum -c - >/dev/null 2>&1; then
    note "✗ CHECKSUM MISMATCH — refusing to install. Expected:"
    note "    $sha"
    note "  got $(sha256sum "$tmp/m.tgz" | cut -d' ' -f1)"
    rm -rf "$tmp"; return 0
  fi
  # The agent bundle has to sit beside the binary — mutagen installs the remote
  # agent from it, so a lone `mutagen` can't act as a client.
  if ! tar -xzf "$tmp/m.tgz" -C "$tmp" mutagen mutagen-agents.tar.gz; then
    note "✗ extract failed — install mutagen by hand from the URL above."
    rm -rf "$tmp"; return 0
  fi
  # `install`, not `tar -C`: as root, tar honours the archive's own mode and
  # owner, so a hostile tarball could land setuid or world-writable.
  sudo install -m 0755 -o root -g root "$tmp/mutagen" /usr/local/bin/mutagen
  sudo install -m 0644 -o root -g root \
    "$tmp/mutagen-agents.tar.gz" /usr/local/bin/mutagen-agents.tar.gz
  rm -rf "$tmp"
  note "installed /usr/local/bin/mutagen"
}

prerequisites() {
  say "Prerequisites"
  local missing=()
  have_bin podman "runs your container stacks"            || missing+=(podman)
  have_bin ss     "allocates free host ports (iproute2)"  || missing+=(iproute2)
  have_bin curl   "used below to fetch mutagen"           || missing+=(curl)
  have_bin sshd   "the transport ws drives this box over" || missing+=(openssh-server)
  # `podman compose` is only a delegating front-end — without a real Compose
  # binary beside it, every `wsx up` fails at the compose call.
  if compose_provider_present; then
    note "✓ compose provider"
  else
    note "✗ compose provider — the Compose binary \`podman compose\` calls"
    missing+=(docker-compose)
  fi

  if [ "${#missing[@]}" -gt 0 ]; then
    note "installing: ${missing[*]}"
    sudo apt-get update -qq
    sudo apt-get install -y "${missing[@]}"
  fi

  # Kept last: it's the one that isn't an apt package.
  have_bin mutagen "receives the worktree file sync" || install_mutagen

  check_compose
}

# Having the binaries isn't enough — they have to be new enough to understand the
# override file `ws` generates. Debian 12 fails this on both counts (podman 4.3.1
# has no `compose` subcommand at all; its docker-compose is the 1.x line), so say
# so plainly rather than letting `wsx up` fail later with a YAML parse error.
check_compose() {
  local v
  if ! podman compose version >/dev/null 2>&1; then
    note "✗ \`podman compose\` does not work on this box."
    note "    ws needs podman >= 4.7 (older ones have no \`compose\` subcommand)"
    note "    and a Compose provider >= $COMPOSE_MIN (for the \`!override\` tag)."
    note "    Debian 12 ships podman 4.3.1 + docker-compose 1.29.2 — both too old."
    note "    Use Debian 13 (trixie) or newer, or install them from upstream."
    note "    Container repos will not work until this is resolved."
    return 0
  fi
  # `|| true`: not every provider implements `--short`, and pipefail would
  # otherwise abort the whole script over a version string we can live without.
  v="$(podman compose version --short 2>/dev/null || true)"
  v="${v#v}"
  if [ -n "$v" ] &&
     [ "$(printf '%s\n%s\n' "$COMPOSE_MIN" "$v" | sort -V | head -1)" != "$COMPOSE_MIN" ]; then
    note "✗ Compose $v is older than $COMPOSE_MIN — \`!override\` is unsupported."
    note "    Container repos will fail at \`wsx up\` until this is upgraded."
    return 0
  fi
  note "✓ podman compose${v:+ (provider $v)}"
}

# ---------------------------------------------------------------------------
# 1. Trust Caddy's internal root CA
# ---------------------------------------------------------------------------
# Every certificate in the blob we install gets trusted machine-wide, so take
# only a single CA and show it before trusting it. Guards the mundane case (a
# truncated read putting a broken entry in the trust store) as well as a Caddy
# container that returns the expected root plus an extra CA of its own.
check_ca() {
  local pem="$1" count
  if ! command -v openssl >/dev/null 2>&1; then
    note "skip: openssl not available to verify the certificate before trusting it."
    return 1
  fi
  count="$(printf '%s\n' "$pem" | grep -c '^-----BEGIN CERTIFICATE-----' || true)"
  if [ "$count" != "1" ]; then
    note "skip: expected exactly one certificate from $CADDY_CONTAINER, got $count."
    return 1
  fi
  if ! printf '%s\n' "$pem" | openssl x509 -noout -ext basicConstraints 2>/dev/null |
       grep -q 'CA:TRUE'; then
    note "skip: that certificate is not a CA — refusing to add it to the trust store."
    return 1
  fi
  note "certificate: $(printf '%s\n' "$pem" | openssl x509 -noout -subject 2>/dev/null)"
  note "sha256:      $(printf '%s\n' "$pem" |
    openssl x509 -noout -fingerprint -sha256 2>/dev/null | cut -d= -f2)"
  return 0
}

trust_ca() {
  say "Trust Caddy's root CA"
  if ! podman container inspect "$CADDY_CONTAINER" >/dev/null 2>&1; then
    note "skip: the $CADDY_CONTAINER container isn't running yet."
    note "      start it from the machine running \`ws\` (\`wsx setup\`, which reaches"
    note "      this box over SSH when it's remote), then re-run this script."
    return 0
  fi
  local root
  root="$(podman exec "$CADDY_CONTAINER" \
    cat /data/caddy/pki/authorities/local/root.crt 2>/dev/null || true)"
  if [ -z "$root" ]; then
    note "skip: couldn't read Caddy's root CA from $CADDY_CONTAINER."
    return 0
  fi
  if ! check_ca "$root"; then
    return 0
  fi
  # Compare with the same trailing newline the file is written with (below), so
  # an unchanged cert is correctly detected as already-installed (idempotent).
  if [ -f "$CA_DEST" ] && printf '%s\n' "$root" | sudo cmp -s - "$CA_DEST"; then
    note "already trusted ($CA_DEST) — nothing to do."
    return 0
  fi
  printf '%s\n' "$root" | sudo tee "$CA_DEST" >/dev/null
  sudo update-ca-certificates
  note "installed $CA_DEST and refreshed the system trust store."
}

# ---------------------------------------------------------------------------
# 2. Wildcard DNS  *.<domain> → <address>  (via dnsmasq)
# ---------------------------------------------------------------------------
# The safe, scriptable part — install dnsmasq and drop in the wildcard answer —
# is done here. Pointing the system resolver at dnsmasq is environment-specific,
# so that last hop is printed, not forced.
wildcard_dns() {
  say "Wildcard DNS: *.$domain → $address"
  local conf="/etc/dnsmasq.d/ws-${domain}.conf"
  # dnsmasq lives in /usr/sbin, off a normal user's PATH — checking the name
  # alone would reinstall it on every run.
  if ! PATH="$PATH:/usr/sbin:/sbin" command -v dnsmasq >/dev/null 2>&1; then
    note "installing dnsmasq…"
    sudo apt-get update -qq
    sudo apt-get install -y dnsmasq
  fi
  # `bind-interfaces` matters more than it looks: left to itself dnsmasq claims
  # :53 on *every* interface, including the gateway of podman's container
  # network. aardvark-dns then can't bind, so the ws network — and the Caddy
  # container on it — fails to start outright. Listen only where clients ask.
  local desired="address=/${domain}/${address}
listen-address=127.0.0.1"
  [ "$address" = "127.0.0.1" ] || desired="${desired}
listen-address=${address}"
  desired="${desired}
bind-interfaces"
  if [ -f "$conf" ] && [ "$(cat "$conf")" = "$desired" ]; then
    note "already configured ($conf)."
  else
    printf '%s\n' "$desired" | sudo tee "$conf" >/dev/null
    # Report a failed restart: dnsmasq bails out on a config it dislikes, and a
    # silent failure here leaves the box with no resolver at all.
    if sudo systemctl restart dnsmasq 2>/dev/null ||
       sudo service dnsmasq restart >/dev/null 2>&1; then
      note "wrote $conf and restarted dnsmasq."
    else
      note "wrote $conf, but dnsmasq FAILED to restart."
      note "  check \`systemctl status dnsmasq\` — DNS may be down until it's fixed."
    fi
  fi
  note "final hop — point the machine running \`ws\` at this resolver:"
  if [ -L /etc/resolv.conf ] && readlink /etc/resolv.conf | grep -q systemd; then
    note "  systemd-resolved detected → route just '$domain' here:"
    note "    sudo resolvectl dns    <iface> ${address}"
    note "    sudo resolvectl domain <iface> '~${domain}'"
  else
    note "  put '${address}' first in /etc/resolv.conf, or add a"
    note "  \`address=/${domain}/${address}\` line to that machine's own dnsmasq."
  fi
  if [ "$address" = "127.0.0.1" ]; then
    note "note: answering with loopback — right when \`ws\` runs on THIS box."
    note "      for an offload box, re-run with \`--address <ip clients reach it at>\`."
  fi
}

# ---------------------------------------------------------------------------
# 2b. Make THIS machine resolve *.<domain> too
# ---------------------------------------------------------------------------
# Installing dnsmasq without wiring the resolver looks complete and isn't: the
# box ends up running a correct DNS server that nothing queries.
#
# It went unnoticed because nothing here ever needed these names. Caddy matches
# on the Host header and containers talk over the compose network, so the server
# was never a client of its own routing. A `kind: host` repo changes that — a
# dev server running here dials `<branch>.<svc>.<domain>` to reach a routed
# container, and that name has to resolve locally first.
#
# The ordering is the entire risk, which is why this belongs in a script rather
# than a printed instruction. dnsmasq takes its own upstreams from
# /etc/resolv.conf by default, so pointing that file at dnsmasq *before* giving
# it an explicit upstream makes it forward queries to itself and DNS stops
# working for the whole machine. Capture the upstream first, switch, verify, and
# restore on failure.
local_resolver() {
  say "Local resolver: resolve *.$domain on this machine"

  if getent hosts "ws-probe.${domain}" >/dev/null 2>&1; then
    note "already resolves here — nothing to do."
    # Resolving *now* says nothing about surviving the next lease renewal, and an
    # earlier run of this script wired the file without defending it. Re-running
    # is how that box gets the guard, so don't skip the step just because the
    # lookup works at this moment.
    persist_resolver
    return 0
  fi

  # systemd-resolved can route a single domain, leaving every other lookup on
  # the existing upstream. Narrowest blast radius available, so prefer it.
  if [ -L /etc/resolv.conf ] && readlink /etc/resolv.conf | grep -q systemd; then
    local unit="/etc/systemd/resolved.conf.d/ws-${domain}.conf"
    note "systemd-resolved detected → routing only '$domain' at dnsmasq."
    sudo mkdir -p /etc/systemd/resolved.conf.d
    printf '[Resolve]\nDNS=127.0.0.1\nDomains=~%s\n' "$domain" | sudo tee "$unit" >/dev/null
    if sudo systemctl restart systemd-resolved 2>/dev/null; then
      note "wrote $unit."
    else
      note "wrote $unit, but systemd-resolved FAILED to restart."
      note "  check \`systemctl status systemd-resolved\`."
    fi
    getent hosts "ws-probe.${domain}" >/dev/null 2>&1 ||
      note "still not resolving — check \`resolvectl status\`."
    return 0
  fi

  # Plain /etc/resolv.conf. Whether an external name resolves *now* is the bar
  # to clear afterwards — an offline box shouldn't fail this step for being
  # offline, so only require what already worked.
  local canary="deb.debian.org" external_ok=0
  getent hosts "$canary" >/dev/null 2>&1 && external_ok=1

  local upstreams
  upstreams="$(awk '/^[[:space:]]*nameserver/ { print $2 }' /etc/resolv.conf | grep -v '^127\.' || true)"
  if [ -z "$upstreams" ]; then
    note "no non-loopback nameserver in /etc/resolv.conf — skipping."
    note "  give dnsmasq an upstream by hand before pointing this box at it,"
    note "  or it will forward queries to itself."
    return 0
  fi

  # A separate file: `wildcard_dns` rewrites its own conf whole whenever it
  # differs, which would drop these lines on the next run.
  local up_conf="/etc/dnsmasq.d/ws-upstream.conf"
  { echo "# Written by server-setup — dnsmasq must not read /etc/resolv.conf"
    echo "# for upstreams once that file points back here."
    echo "no-resolv"
    # Unquoted on purpose: `$upstreams` is newline-separated and must split into
    # one `server=` line per nameserver. Quoting it emits a single broken line.
    printf 'server=%s\n' $upstreams
  } | sudo tee "$up_conf" >/dev/null
  if ! sudo systemctl restart dnsmasq 2>/dev/null &&
     ! sudo service dnsmasq restart >/dev/null 2>&1; then
    note "dnsmasq FAILED to restart — leaving the resolver untouched."
    sudo rm -f "$up_conf"
    return 0
  fi

  local backup="/etc/resolv.conf.ws-backup"
  sudo cp /etc/resolv.conf "$backup"
  # An earlier run may have frozen the file (see `freeze_resolver`); both the
  # write below and the restore on failure fail outright while it is immutable.
  thaw_resolver
  # Keep the old servers as fallbacks. glibc only falls through on timeout, not
  # on NXDOMAIN, so this can't leak `*.$domain` to the upstream — it only means
  # a dead dnsmasq degrades to the previous behaviour instead of no DNS at all.
  { echo "# ws: dnsmasq first for *.${domain}; originals kept as fallback."
    echo "nameserver 127.0.0.1"
    printf 'nameserver %s\n' $upstreams   # unquoted: one line per server
  } | sudo tee /etc/resolv.conf >/dev/null

  local ok=1
  getent hosts "ws-probe.${domain}" >/dev/null 2>&1 || ok=0
  [ "$external_ok" = 0 ] || getent hosts "$canary" >/dev/null 2>&1 || ok=0
  if [ "$ok" = 1 ]; then
    note "resolver now consults dnsmasq (backup at $backup)."
    persist_resolver
  else
    sudo cp "$backup" /etc/resolv.conf
    note "verification FAILED — restored the previous /etc/resolv.conf."
    note "  dnsmasq keeps $up_conf; resolve *.${domain} by hand before retrying."
  fi
}

# ---------------------------------------------------------------------------
# 2c. Keep that wiring across a DHCP lease renewal
# ---------------------------------------------------------------------------
# /etc/resolv.conf is not ours to own. Whatever configures the interface rewrites
# it whenever the lease renews, and writing the file once then walking away looks
# permanent for exactly as long as the lease lasts.
#
# From a real incident: dhclient renewed an ~11h lease, replaced the file with
# the router's nameserver, and dropped the loopback line. dnsmasq stayed up and
# answered correctly when asked directly — but nothing asked it, so a `kind: host`
# dev server on this box started failing every proxy hop to a routed container
# with `getaddrinfo ENOTFOUND`. Twice a day, self-inflicted, silent.
#
# Each network manager wants a different mechanism, so detect which one is in
# play rather than assume. Where it isn't determinable, or where the fix means
# editing a profile only the operator can pick, print the exact thing to do
# instead of guessing at someone else's network config.

# Whether /etc/resolv.conf carries the immutable attribute.
resolver_frozen() {
  command -v lsattr >/dev/null 2>&1 || return 1
  lsattr /etc/resolv.conf 2>/dev/null | awk '{print $1}' | grep -q i
}

# Drop that attribute so this script can rewrite the file — a no-op when it was
# never frozen. Anything here that writes /etc/resolv.conf must go through this
# first, or an earlier run's freeze turns the write into a fatal error.
thaw_resolver() {
  resolver_frozen || return 0
  sudo chattr -i /etc/resolv.conf
}

# Freeze /etc/resolv.conf so that nothing can rewrite it. Non-zero when that
# isn't safe or possible here, which falls the caller through to the
# per-manager guidance below.
#
# Safe only once dnsmasq has `no-resolv`: it then takes its upstreams from its
# own config, which leaves /etc/resolv.conf with no remaining job but to name
# the local resolver — a static fact with nothing left to learn from a lease.
# Every other mechanism below *races* the rewrite (get the right line into the
# right DHCP client's config, then hope that client re-reads it); freezing
# removes the race instead of winning it.
#
# A symlinked resolv.conf is left alone — resolvconf/openresolv keep the real
# file elsewhere, so the attribute would land on the link and defend nothing.
freeze_resolver() {
  local up_conf="/etc/dnsmasq.d/ws-upstream.conf"
  grep -qE '^[[:space:]]*no-resolv[[:space:]]*$' "$up_conf" 2>/dev/null || return 1
  [ ! -L /etc/resolv.conf ] || return 1
  command -v chattr >/dev/null 2>&1 || return 1

  if resolver_frozen; then
    note "/etc/resolv.conf is already frozen — no renewal can rewrite it."
  else
    # Not every filesystem carries the attribute; that's a fallthrough to the
    # advice below, not a failure worth aborting the whole setup for.
    if ! sudo chattr +i /etc/resolv.conf 2>/dev/null; then
      note "couldn't set the immutable attribute on /etc/resolv.conf"
      note "  (the filesystem may not support it) — falling back."
      return 1
    fi
    note "froze /etc/resolv.conf — no lease renewal can rewrite it."
  fi
  note "  dnsmasq has \`no-resolv\`, so its upstreams live in $up_conf and this"
  note "  file only has to name the local resolver."
  note "  to edit it later:  sudo chattr -i /etc/resolv.conf"
  note "  if this box moves to another network, update the \`server=\` line in"
  note "  $up_conf too — \`no-resolv\` means DHCP no longer supplies it."
  return 0
}

persist_resolver() {
  # systemd-resolved owns the file and the drop-in written above is persistent
  # config, not a file DHCP rewrites — nothing to defend.
  if [ -L /etc/resolv.conf ] && readlink /etc/resolv.conf | grep -q systemd; then
    return 0
  fi
  # Only meaningful once this box points at its own dnsmasq through that file.
  grep -qE '^[[:space:]]*nameserver[[:space:]]+127\.' /etc/resolv.conf 2>/dev/null || return 0

  say "Keep /etc/resolv.conf pointing at dnsmasq across DHCP renewals"

  # Freezing the file outright is the strongest option and the only one that
  # doesn't depend on which DHCP client runs here, on when it last parsed its
  # config, or on it being restarted afterwards — so try it before the
  # per-manager mechanisms. That dependency is exactly what failed in practice:
  # the `prepend` below was correct on disk and ignored for a month by a
  # dhclient that had started before the line existed.
  if freeze_resolver; then
    return 0
  fi

  if systemctl is-active --quiet NetworkManager 2>/dev/null; then
    # NetworkManager uses its own internal DHCP client by default, which doesn't
    # read /etc/dhcp/dhclient.conf — the fix belongs on the connection profile
    # instead. Not applied here: which profile owns this box's route is a choice,
    # and modifying the wrong one takes the network down.
    note "NetworkManager is managing this box's networking, and it rewrites"
    note "  /etc/resolv.conf on renewal. Pin the resolver on the connection —"
    note "  a manual DNS entry sorts ahead of the DHCP-supplied ones, which stay"
    note "  behind it as fallback:"
    local conn
    conn="$(nmcli -t -f NAME connection show --active 2>/dev/null | head -n1 || true)"
    note "    sudo nmcli connection modify '${conn:-<connection>}' +ipv4.dns 127.0.0.1"
    note "    sudo nmcli connection up     '${conn:-<connection>}'"
    return 0
  fi

  # dhclient (Debian's default with ifupdown). `prepend` is applied every time
  # dhclient regenerates the file, which is exactly the event that broke it.
  if [ -f /etc/dhcp/dhclient.conf ] &&
     { pgrep -x dhclient >/dev/null 2>&1 ||
       PATH="$PATH:/usr/sbin:/sbin" command -v dhclient >/dev/null 2>&1; }; then
    local dh="/etc/dhcp/dhclient.conf"
    if grep -qE '^[[:space:]]*prepend[[:space:]]+domain-name-servers[[:space:]]+127\.0\.0\.1[[:space:]]*;' "$dh"; then
      note "dhclient already prepends 127.0.0.1 ($dh)."
    else
      { echo
        echo "# ws: put dnsmasq first every time dhclient rewrites resolv.conf."
        echo "# The DHCP-supplied servers stay behind it as fallback."
        echo "prepend domain-name-servers 127.0.0.1;"
      } | sudo tee -a "$dh" >/dev/null
      note "added \`prepend domain-name-servers 127.0.0.1;\` to $dh."
    fi
    # dhclient parses its config at startup only — a renewal re-applies what the
    # running process read back then, so a dhclient older than the prepend line
    # keeps rewriting resolv.conf without it until restarted.
    local pid started
    pid="$(pgrep -o -x dhclient 2>/dev/null || true)"
    if [ -z "$pid" ]; then
      note "no dhclient is running right now — the prepend applies when one starts."
      return 0
    fi
    started="$(( $(date +%s) - $(ps -o etimes= -p "$pid" | tr -d ' ') ))"
    if [ "$started" -lt "$(stat -c %Y "$dh")" ]; then
      note "the running dhclient (pid $pid) started before that line existed and"
      note "  won't read it until restarted — until then a lease renewal still"
      note "  rewrites /etc/resolv.conf without dnsmasq. Restart it in place"
      note "  (the lease and the interface stay up):"
      note "    sudo kill $pid"
      note "    sudo $(tr '\0' ' ' <"/proc/$pid/cmdline")"
    fi
    return 0
  fi

  if systemctl is-active --quiet systemd-networkd 2>/dev/null; then
    note "systemd-networkd is managing the interfaces. Pin the resolver in the"
    note "  matching /etc/systemd/network/*.network file — a static DNS= is used"
    note "  ahead of the DHCP-supplied servers, which remain as fallback:"
    note "    [Network]"
    note "    DNS=127.0.0.1"
    note "  then: sudo systemctl restart systemd-networkd"
    return 0
  fi

  note "couldn't tell what rewrites /etc/resolv.conf here — no NetworkManager,"
  note "  dhclient or systemd-networkd found. If this box takes a DHCP lease,"
  note "  the renewal will drop the line just written."
  note "  check the file after the next renewal; if nothing owns it, freeze it:"
  note "    sudo chattr +i /etc/resolv.conf"
}

# ---------------------------------------------------------------------------
# 3. (opt-in) Allow rootless bind to :443
# ---------------------------------------------------------------------------
unprivileged_443() {
  [ "$want_443" -eq 1 ] || return 0
  say "Allow rootless bind to :443"
  local start
  start="$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start 2>/dev/null || echo 1024)"
  if [ "$start" -le 443 ]; then
    note "already permitted (ip_unprivileged_port_start=$start)."
  else
    echo 'net.ipv4.ip_unprivileged_port_start=443' | sudo tee "$SYSCTL_FILE" >/dev/null
    sudo sysctl --system >/dev/null
    note "set net.ipv4.ip_unprivileged_port_start=443 ($SYSCTL_FILE)."
  fi
  note "then, from the machine running \`ws\`: \`wsx setup --port 443\` to move this"
  note "box's Caddy onto :443 (it re-seeds)."
}

# ---------------------------------------------------------------------------
# 4. Linger — give the login-less SSH sessions a `systemd --user` manager
# ---------------------------------------------------------------------------
# Not for host processes (`ws` supervises those itself, no service manager
# involved) — for the rootless podman API socket below, which *is* a
# `systemd --user` unit. Without lingering, that user manager only exists while
# a session is open, and `ws` connects over non-login SSH.
enable_linger() {
  # `id -un`, not `$USER`: the latter is just an env var, so a stray value would
  # aim this script's `sudo` at somebody else's account.
  local user
  user="$(id -un)"
  say "Enable linger for $user"
  if loginctl show-user "$user" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
    note "already enabled."
  else
    sudo loginctl enable-linger "$user"
    note "enabled — the \`systemd --user\` manager now runs without a session."
  fi
}

# ---------------------------------------------------------------------------
# 5. The podman API socket — what `podman compose` actually talks to
# ---------------------------------------------------------------------------
# `podman compose` hands the work to Docker's Compose binary, which speaks the
# Docker API over podman's socket. Without it every `wsx up` dies on "Cannot
# connect to the Docker daemon". Runs after linger because the rootless socket
# is a `systemd --user` unit, and `ws` connects over a non-login SSH session
# where the user manager only exists if lingering is on.
podman_socket() {
  say "Podman API socket"
  local path
  path="$(podman info --format '{{.Host.RemoteSocket.Path}}' 2>/dev/null || true)"
  # Test the path itself. podman's own `RemoteSocket.Exists` reports true for a
  # socket that isn't there, so trusting it skips the work and `wsx up` then
  # fails with a bare "Cannot connect to the Docker daemon".
  if [ -n "$path" ] && [ -S "$path" ]; then
    note "already listening ($path)."
    return 0
  fi
  # Ask podman which socket it wants rather than guessing: rootful podman uses
  # the system unit, rootless the per-user one.
  case "$path" in
    /run/user/*)
      if systemctl --user enable --now podman.socket >/dev/null 2>&1; then
        note "enabled the user podman.socket ($path)."
      else
        note "✗ couldn't enable it — run this as the user that owns the stacks:"
        note "    systemctl --user enable --now podman.socket"
      fi
      ;;
    *)
      if sudo systemctl enable --now podman.socket >/dev/null 2>&1; then
        note "enabled the system podman.socket (${path:-/run/podman/podman.sock})."
      else
        note "✗ couldn't enable it — try: sudo systemctl enable --now podman.socket"
      fi
      ;;
  esac
}

prerequisites
trust_ca
wildcard_dns
local_resolver
unprivileged_443
enable_linger
podman_socket

# Deliberately doesn't name a `ws` command to run *here*: this box hosts the
# stacks, and on a dedicated one `ws` isn't installed — the client drives it over
# SSH. `wsx remote check` is the right call from wherever `ws` lives, reporting on
# the server when a `remote` block exists and on the local host otherwise.
#
# Say that `ws` may not be here, without claiming to know. Phrasing this as a bare
# "re-check from the machine running ws" reads, to someone sitting on the server
# that just printed it, as an instruction to run a command that isn't installed.
say "Done. Re-check with \`wsx remote check\`, run from wherever you use \`ws\`"
say "— on a dedicated offload box that's your client machine, not this one."
