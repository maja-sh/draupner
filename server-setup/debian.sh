#!/usr/bin/env bash
# ws host setup — Debian / Ubuntu
# ================================
#
# The privileged, one-time bits `ws` deliberately does NOT run for you. `ws`
# handles the rootless pieces itself (the `ws-net` network, the Caddy container);
# these three need root, so they live here — a readable script you run yourself.
# That's the transparency contract: nothing privileged happens invisibly.
#
#   1. Trust Caddy's internal root CA  → browsers stop warning on the TLS cert.
#   2. Wildcard DNS  *.<domain> → 127.0.0.1  → per-worktree hostnames resolve.
#   3. (opt-in) Rootless :443  → drop the `:8443` suffix from URLs.
#
# Plus `loginctl enable-linger` so `systemd --user` host-process units survive
# logout. Every step is idempotent — safe to re-run.
#
# Usage:
#   server-setup/debian.sh <domain> [--port 443]
#   WS_DOMAIN=myapp.test server-setup/debian.sh
#
# Run it as your normal user; it invokes `sudo` per privileged command (so sudo
# prompts you directly — the script never reads your password). Caddy must be up
# first (`ws setup` or `ws up`) so its root CA exists.

set -euo pipefail

CADDY_CONTAINER="ws-caddy"
CA_DEST="/usr/local/share/ca-certificates/ws-caddy-root.crt"
SYSCTL_FILE="/etc/sysctl.d/99-ws-unprivileged-ports.conf"

runtime="${WS_RUNTIME:-podman}"
domain="${WS_DOMAIN:-}"
want_443=0

while [ $# -gt 0 ]; do
  case "$1" in
    # Only treat `--port 443` / `--port=443` as the opt-in; consume the `443`
    # token when present, but never swallow a non-port argument (e.g. the domain).
    --port) case "${2:-}" in 443) want_443=1; shift ;; esac ;;
    --port=443) want_443=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) domain="$1" ;;
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

say()  { printf '\n\033[1m▸ %s\033[0m\n' "$*"; }
note() { printf '  %s\n' "$*"; }

# ---------------------------------------------------------------------------
# 1. Trust Caddy's internal root CA
# ---------------------------------------------------------------------------
trust_ca() {
  say "Trust Caddy's root CA"
  if ! "$runtime" container inspect "$CADDY_CONTAINER" >/dev/null 2>&1; then
    note "skip: the $CADDY_CONTAINER container isn't running yet."
    note "      start it with \`ws setup\` (or \`ws up <branch>\`), then re-run."
    return 0
  fi
  local root
  root="$("$runtime" exec "$CADDY_CONTAINER" \
    cat /data/caddy/pki/authorities/local/root.crt 2>/dev/null || true)"
  if [ -z "$root" ]; then
    note "skip: couldn't read Caddy's root CA from $CADDY_CONTAINER."
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
# 2. Wildcard DNS  *.<domain> → 127.0.0.1  (via dnsmasq)
# ---------------------------------------------------------------------------
# The safe, scriptable part — install dnsmasq and drop the wildcard answer — is
# done here. Pointing the system resolver at dnsmasq is environment-specific
# (and, on an offload box, is usually a Tailscale Split-DNS entry rather than a
# local resolv.conf change), so that last hop is printed, not forced.
wildcard_dns() {
  say "Wildcard DNS: *.$domain → 127.0.0.1"
  local conf="/etc/dnsmasq.d/ws-${domain}.conf"
  if ! command -v dnsmasq >/dev/null 2>&1; then
    note "installing dnsmasq…"
    sudo apt-get update -qq
    sudo apt-get install -y dnsmasq
  fi
  local line="address=/${domain}/127.0.0.1"
  if [ -f "$conf" ] && grep -qxF "$line" "$conf"; then
    note "already configured ($conf)."
  else
    printf '%s\n' "$line" | sudo tee "$conf" >/dev/null
    sudo systemctl restart dnsmasq 2>/dev/null || sudo service dnsmasq restart || true
    note "wrote $conf and restarted dnsmasq."
  fi
  note "final hop — make the system resolve through dnsmasq:"
  if [ -L /etc/resolv.conf ] && readlink /etc/resolv.conf | grep -q systemd; then
    note "  systemd-resolved detected → add a split-DNS route for '$domain':"
    note "    sudo resolvectl dns  <iface> 127.0.0.1"
    note "    sudo resolvectl domain <iface> '~${domain}'"
  else
    note "  ensure '127.0.0.1' is the first nameserver in /etc/resolv.conf"
    note "  (or, on a Tailscale offload box, add a Split-DNS entry:"
    note "   '${domain}' → this box's tailnet IP in the Tailscale admin console)."
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
  note "then: \`ws setup --port 443\` to move Caddy onto :443 (it re-seeds)."
}

# ---------------------------------------------------------------------------
# 4. Linger — keep `systemd --user` host units alive past logout
# ---------------------------------------------------------------------------
enable_linger() {
  # `$USER` isn't always exported (cron, minimal shells); fall back to `id -un`
  # so `set -u` doesn't abort here.
  local user="${USER:-$(id -un)}"
  say "Enable linger for $user"
  if loginctl show-user "$user" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
    note "already enabled."
  else
    sudo loginctl enable-linger "$user"
    note "enabled — \`systemd --user\` units now survive logout."
  fi
}

trust_ca
wildcard_dns
unprivileged_443
enable_linger

say "Done. Re-check readiness any time with \`ws setup\`."
