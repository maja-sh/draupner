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
#   0. Prerequisites  → podman, a Compose provider, iproute2, mutagen, curl, sshd.
#   1. Trust Caddy's internal root CA  → browsers stop warning on the TLS cert.
#   2. Wildcard DNS  *.<domain> → <address>  → per-worktree hostnames resolve.
#   2b. Local resolver  → this box resolves them too, which a `kind: host` repo
#       needs to reach a routed container from a dev server running here.
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
# first so its root CA exists — `ws setup` or `ws up` starts it, run on whichever
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

runtime="${WS_RUNTIME:-podman}"
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
is_offload() { [ "$address" != "127.0.0.1" ]; }

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
# Host processes (`kind: host` repos) additionally need a `systemd --user`
# session, which the linger step at the bottom of this script sets up.

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
  # binary beside it, every `ws up` fails at the compose call.
  if compose_provider_present; then
    note "✓ compose provider"
  else
    note "✗ compose provider — the Compose binary \`$runtime compose\` calls"
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
# so plainly rather than letting `ws up` fail later with a YAML parse error.
check_compose() {
  local v
  if ! "$runtime" compose version >/dev/null 2>&1; then
    note "✗ \`$runtime compose\` does not work on this box."
    note "    ws needs $runtime >= 4.7 (older ones have no \`compose\` subcommand)"
    note "    and a Compose provider >= $COMPOSE_MIN (for the \`!override\` tag)."
    note "    Debian 12 ships podman 4.3.1 + docker-compose 1.29.2 — both too old."
    note "    Use Debian 13 (trixie) or newer, or install them from upstream."
    note "    Container repos will not work until this is resolved."
    return 0
  fi
  # `|| true`: not every provider implements `--short`, and pipefail would
  # otherwise abort the whole script over a version string we can live without.
  v="$("$runtime" compose version --short 2>/dev/null || true)"
  v="${v#v}"
  if [ -n "$v" ] &&
     [ "$(printf '%s\n%s\n' "$COMPOSE_MIN" "$v" | sort -V | head -1)" != "$COMPOSE_MIN" ]; then
    note "✗ Compose $v is older than $COMPOSE_MIN — \`!override\` is unsupported."
    note "    Container repos will fail at \`ws up\` until this is upgraded."
    return 0
  fi
  note "✓ $runtime compose${v:+ (provider $v)}"
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
  if ! "$runtime" container inspect "$CADDY_CONTAINER" >/dev/null 2>&1; then
    note "skip: the $CADDY_CONTAINER container isn't running yet."
    if is_offload; then
      note "      start it from the machine running \`ws\` (\`ws setup\` reaches this"
      note "      box over SSH), then re-run this script."
    else
      note "      start it with \`ws setup\` (or \`ws up <branch>\`), then re-run."
    fi
    return 0
  fi
  local root
  root="$("$runtime" exec "$CADDY_CONTAINER" \
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
    if [ -f /etc/dhcp/dhclient.conf ] || pgrep -x dhclient >/dev/null 2>&1; then
      note "warning: dhclient may rewrite /etc/resolv.conf on lease renewal."
      note "  make it stick with: sudo chattr +i /etc/resolv.conf"
    fi
  else
    sudo cp "$backup" /etc/resolv.conf
    note "verification FAILED — restored the previous /etc/resolv.conf."
    note "  dnsmasq keeps $up_conf; resolve *.${domain} by hand before retrying."
  fi
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
  if is_offload; then
    note "then, from the machine running \`ws\`: \`ws setup --port 443\` to move this"
    note "box's Caddy onto :443 (it re-seeds)."
  else
    note "then: \`ws setup --port 443\` to move Caddy onto :443 (it re-seeds)."
  fi
}

# ---------------------------------------------------------------------------
# 4. Linger — keep `systemd --user` host units alive past logout
# ---------------------------------------------------------------------------
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
    note "enabled — \`systemd --user\` units now survive logout."
  fi
}

# ---------------------------------------------------------------------------
# 5. The podman API socket — what `podman compose` actually talks to
# ---------------------------------------------------------------------------
# `podman compose` hands the work to Docker's Compose binary, which speaks the
# Docker API over podman's socket. Without it every `ws up` dies on "Cannot
# connect to the Docker daemon". Runs after linger because the rootless socket
# is a `systemd --user` unit, and `ws` connects over a non-login SSH session
# where the user manager only exists if lingering is on.
podman_socket() {
  [ "$runtime" = "podman" ] || return 0
  say "Podman API socket"
  local path
  path="$("$runtime" info --format '{{.Host.RemoteSocket.Path}}' 2>/dev/null || true)"
  # Test the path itself. podman's own `RemoteSocket.Exists` reports true for a
  # socket that isn't there, so trusting it skips the work and `ws up` then
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

if is_offload; then
  # `ws` is deliberately NOT installed here — an offload box runs the stacks, the
  # client drives them over SSH. Naming a `ws` command to run *on this machine*
  # would be telling the operator to install the one thing this design avoids.
  say "Done. This box is an offload server, so \`ws\` doesn't run here —"
  say "re-check from the machine running \`ws\`: \`ws remote check\`."
else
  say "Done. Re-check readiness any time with \`ws setup\`."
fi
