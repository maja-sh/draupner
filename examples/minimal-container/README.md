# minimal-container

> **This example's `.drup.json` is checked by CI. The compose file is
> illustrative — not executed in CI. If `drup up` doesn't work on your
> machine, open an issue.**

The floor: one compose repo, no declared services, no scripts, no
`.drup-hooks/`. Everything drup needs it derives.

## What's here

```
minimal-container/
├── .drup.json              # 3 lines of config
└── api/
    └── docker-compose.yml  # one `web` service on :80
```

`api/` is an illustrative source tree, not a git repo. In a real
workspace it would be a clone of your API repo sitting next to
`.drup.json`.

## What drup derives

Running `drup up <branch>` here would:

- Infer `kind: container` from the presence of `docker-compose.yml`.
- Read the `web` service and its port from compose config.
- Compose-up the stack under a per-branch project name.
- Route `<branch>.web.myapp.test` to the container's `:80`.
- Fall back to `<runtime> compose up -d` / `down` / `ps` for the
  lifecycle, since no `scripts:` block overrides them.

No `.drup-hooks/` is needed either; drup's compose defaults handle
`up`, `down`, `status`, and `post-remove`.

## The point

Most `.drup.json` fields are optional. If your repo already has a
working `docker-compose.yml`, three lines of config and a `domain`
are enough.
