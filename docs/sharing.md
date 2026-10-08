# Sharing a task on your network

Sometimes someone else needs to use the thing you're building — a tester, a
reviewer, a support person — and the closest you have to a staging environment
is the stack running on your machine. `wsx share` makes one task reachable from
other machines on your network, and nothing else:

```sh
wsx share feat/checkout     # this task, reachable from the LAN
wsx unshare feat/checkout   # private again
```

Sharing is per task. Every other task stays loopback-only, and a hostname that
isn't shared gets a 503 from the shared address rather than your stack.

---

## How it works

Caddy gets a second listener that carries only shared tasks' routes. It's
published on your machine's network address, on the same port as the private
one, so a URL is identical from your browser and from theirs:

```
https://feat-checkout.app.dev.example.com:8443
```

The listener only exists while something is shared. Unsharing the last task
(or removing it with `wsx rm`) unpublishes it again. A shared task that's down
stays shared: the next `wsx up` brings its routes back to the shared listener.

Changing what's published means recreating the `ws-caddy` container. Routes
survive that, but every routed stack on the machine drops for a second or so.

The address is your default route's source address. On a machine with several
networks, pin the one the other machines are on:

```sh
wsx share feat/checkout --address 192.168.1.20
```

The pin is remembered for later shares, until nothing is shared any more.

If your address changes while a task is shared, the listener is still bound to
the old one until something re-binds it. `wsx share --refresh` does exactly that:
it re-derives the address and recreates Caddy if it moved, and it's a quiet
no-op otherwise. Run it from the workspace on a timer, next to whatever keeps
your DNS record current:

```sh
cd ~/work/myapp && wsx share --refresh
```

`wsx share` is for local workspaces. An offloaded workspace's Caddy already
publishes on the server's interfaces. See `remote.publish_address` in
[offload.md](offload.md).

---

## What the other machines need

Three things: a name that resolves to your machine, a certificate they trust,
and a network path to you.

### A domain you control

Set `domain` in `.wsx.json` to a subdomain of a real domain you own, such as
`dev.example.com`. A private TLD like `myapp.test` works for you but leaves
everyone else needing DNS and CA changes on their own machine.

Changing `domain` changes every hostname, so do it while nothing is up.

### DNS

The other machines need `*.<domain>` to resolve to your address. A wildcard
record at your DNS provider is enough:

```
*.dev.example.com.  A  192.168.1.20
```

A DNS wildcard matches any number of labels, so this one record covers every
branch and every service. A record pointing at a private address is safe to
publish: nobody outside your network can route to it. Two things to check:

- **Rebinding protection.** Some routers and resolvers drop public answers that
  contain private addresses (dnsmasq's `stop-dns-rebind`, pfSense, many ISP
  routers). Check from one of the other machines that the name resolves.
- **A stable address.** Give your machine a DHCP reservation, or the record goes
  stale when your lease changes.

Your own machine keeps whatever `wsx setup` wired, usually loopback through a
local resolver for the domain, so it doesn't depend on this record.

### A certificate

Caddy signs everything with its own local CA by default. Only your machine
trusts that CA, and asking other people to install it is a bad idea: that CA
can sign a certificate for any site, not just yours.

So give `wsx` a publicly trusted certificate instead. `wsx` doesn't issue it. Use
whatever ACME client you like, and put the result here:

```
~/.config/wsx/certs/<domain>/fullchain.pem
~/.config/wsx/certs/<domain>/privkey.pem
```

`wsx` pushes them to Caddy on every `wsx up` and `wsx share`, so a renewal takes
effect on the next one. Delete them and Caddy goes back to its own CA. The key
is sent over stdin, never as an argument, but Caddy's saved config on the
`ws-caddy-config` volume holds a copy.

**Which names.** TLS wildcards cover exactly one label, unlike DNS ones.
Hostnames are `<branch>.<service>.<domain>`, so you need one wildcard per routed
service: `*.app.dev.example.com`, `*.api.dev.example.com`, and so on. Those
names never change as branches come and go. `wsx share` prints the list it needs
and warns about any URL the certificate doesn't cover.

**Why wildcards.** Every publicly trusted certificate is published in
Certificate Transparency logs, which anyone can search. A certificate per branch
would publish every branch name. Wildcards publish only the service names.

Wildcards need the DNS-01 challenge, which needs API access to your DNS
provider. Your machine never has to be reachable from the internet. For example,
with [lego](https://go-acme.github.io/lego/):

```sh
lego --dns <provider> --email you@example.com \
  -d '*.app.dev.example.com' -d '*.api.dev.example.com' \
  --path ~/.lego run
mkdir -p ~/.config/wsx/certs/dev.example.com
ln -sf ~/.lego/certificates/_.app.dev.example.com.crt ~/.config/wsx/certs/dev.example.com/fullchain.pem
ln -sf ~/.lego/certificates/_.app.dev.example.com.key ~/.config/wsx/certs/dev.example.com/privkey.pem
```

Renew on a timer (`lego ... renew`). These certificates last 90 days or less.

A service with `wildcard: true` serves `<tenant>.<branch>.<service>.<domain>`.
That's two labels below the service, so no stable wildcard covers it. Tenant
URLs fall back to Caddy's CA for other machines.

### A network path

The other machines have to reach your address on the HTTPS port (`8443` unless
you ran `wsx setup --port`). A host firewall may ask, or need a rule, the first
time something listens there.

---

## What sharing doesn't do

It isn't access control. Anyone who can reach your address and knows a URL gets
the shared task. It's meant for a trusted network, not for exposing anything to
the internet. It also shares whatever your stack serves, so check what data and
dev-mode shortcuts (auth bypasses, debug toolbars) are in it before you hand out
the URL.
