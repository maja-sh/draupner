# templated-config

> **This example's `.drup.json` is checked by CI. The compose file,
> inline scripts, template file, and source stubs are illustrative —
> not executed in CI. If `drup up` doesn't work on your machine, open
> an issue.**

The feature least visible from reading the schema: a committed source
file with `${DRUP_*}` tokens, rendered to a gitignored destination at
`up` time. For any config format drup doesn't natively understand —
`vite.config.ts`, `nginx.conf`, `redis.conf`, a `.env`, anything.

## What's here

```
templated-config/
├── .drup.json
├── api/
│   └── docker-compose.yml      # web service drup routes
└── web/
    ├── .gitignore              # ignores the rendered vite.config.ts
    ├── package.json
    └── vite.config.ts.template # committed; carries `${DRUP_HOST_api.web}` literally
```

The template declaration in `.drup.json`:

```jsonc
"templates": [
  { "src": "vite.config.ts.template", "dest": "vite.config.ts" }
]
```

## How rendering lands

- The `.template` file is **committed** and carries literal `${DRUP_*}`
  tokens — they look like placeholders in git but are not the final
  values.
- The `dest` file (`vite.config.ts`) is **gitignored**. drup writes it
  at the start of each `up`, substituting the tokens with this
  worktree's `DRUP_HOST_*` / `DRUP_PORT_*` values, including cross-repo
  peer values like `${DRUP_HOST_api.web}`.
- A `dest` path that's tracked by git is refused — rendering can't be
  allowed to dirty a committed file.

## The point

The `templates` block is how you keep per-worktree config
format-blind. Any file format works, as long as `${…}` isn't ambiguous
with the format's own syntax (and even then: non-`DRUP_*` names are
copied byte-for-byte, so an nginx `$host` survives untouched).
