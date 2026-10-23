# web-stack

> **This example's `.drup.json` is checked by CI. The compose file,
> inline scripts, and source stubs are illustrative — not executed in
> CI. If `drup up` doesn't work on your machine, open an issue.**

Three repos: an API in a container, a web frontend as a host process,
and a background worker also as a host process. This is the shape drup
earns its keep on — the pieces chain across repos via `${DRUP_*}`
tokens and the whole task spins up or down as one unit.

## What's here

```
web-stack/
├── .drup.json
├── api/
│   └── docker-compose.yml   # web (:80, HTTP) + db (:5432, raw TCP)
├── web/
│   └── package.json         # host process; `pnpm dev` serves on $DRUP_PORT_web
└── worker/
    └── Cargo.toml           # host process; background job runner, no port
```

The inner directories are illustrative source trees, not git repos. In
a real workspace they'd be clones sitting next to `.drup.json`.

## Cross-repo wiring

Two tokens do the connecting work:

- `web` injects `NEXT_PUBLIC_API_URL=https://${DRUP_HOST_api.web}` so
  the frontend's build reaches the sibling API through drup's
  hostname. Routed by the shared Caddy, so any number of branches
  share one HTTPS port.
- `worker` injects `DATABASE_URL=postgres://…:${DRUP_PORT_api.db}/app`
  so the worker reaches the API's Postgres through the per-worktree
  allocated port. Raw TCP services can't share a hostname, so each
  branch's db gets its own port.

The `DRUP_HOST_<repo>.<svc>` / `DRUP_PORT_<repo>.<svc>` form only
substitutes inside `env` block values and templated files — a `.` in
a shell variable name isn't portable, so these never reach a hook as
an env var. A repo's **own** services do reach it as env, as plain
`DRUP_HOST_<svc>` / `DRUP_PORT_<svc>` — that's why the host scripts
above can reference `$DRUP_PORT_web` directly.

## Why inline `scripts`, not `.drup-hooks/`

Both lanes work. Inline scripts keep this example readable at a
glance; `.drup-hooks/<repo>-<event>.sh` files are the right call once
a hook grows past a one-liner. See `docs/hooks.md`.

## The point

Cross-repo env injection is why drup exists. Two services, two different
addressing shapes (hostname vs port), one declarative file.
