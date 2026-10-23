# examples

Three worked-out workspace configs, each in its own directory with a
`.drup.json`, a `README.md`, and illustrative source stubs. Clone, adapt,
or just skim.

## Which should I look at?

- **[minimal-container/](minimal-container/)** — one compose repo, no
  scripts, no `.drup-hooks/`. The floor. Start here to see how little
  `.drup.json` actually needs to be.
- **[web-stack/](web-stack/)** — three repos: an API (`kind: container`)
  plus a web frontend and a worker (both `kind: host`). Cross-repo env
  injection via `${DRUP_HOST_<repo>.<svc>}` and `${DRUP_PORT_<repo>.<svc>}`
  — the shape drup earns its keep on.
- **[templated-config/](templated-config/)** — a committed `.template`
  file rendered to a gitignored destination at `up` time, for config
  whose format drup doesn't natively understand (vite, nginx, redis.conf,
  …).

They're independent, not progressive — pick the one that looks like the
shape you have.

## Validation

Every example's `.drup.json` is loaded by the same `config::load` drup
uses in production and must round-trip through the schema check on each
CI run. The inner source trees aren't git repos and aren't executed in
CI — compose files, inline scripts, and templated source files are
illustrative. If `drup up` doesn't work on one of these on your machine,
open an issue.
