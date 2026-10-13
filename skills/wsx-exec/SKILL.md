---
name: wsx-exec
description: Run a command inside a task's worktree, where the stack lives (locally or on the remote server). Always prefer over invoking podman/docker/npm/build tools directly inside the worktree directory — those miss the injected COMPOSE_PROJECT_NAME/COMPOSE_FILE/WS_* env and won't see the ws-managed stack. Use when running, testing, or debugging code scoped to a specific branch.
---

Invoke via bash:
`wsx exec [--branch <branch>] [<repo>] -- <command> [args…]`

Contract:

- **`<repo>`** — the repo to run in. Omit to let ws infer from the current worktree; name it explicitly if the CWD is outside a worktree, or if the target is a different repo than the CWD.
- **`--branch <branch>`** — the worktree to target. Omit to let ws infer from CWD; name explicitly whenever the CWD isn't inside the target worktree.
- **`--`** — required separator (clap `last=true`). Everything after it is the command, passed through unchanged (including flags — `--` protects them from being parsed by ws).
- **exit code** passes through from the invoked command, unchanged.

Examples:

```
wsx exec --branch feat/x api -- rails console
wsx exec --branch feat/x api -- npm test -- --grep foo
wsx exec api -- bundle exec rspec           # from inside the api worktree
```

Print `wsx`'s stdout/stderr verbatim. Do not summarize.
