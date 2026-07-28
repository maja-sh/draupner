---
name: ws-resume
description: Resume work on a ws worktree by branch — locate it, confirm it exists, and load how to operate it (files, exec, status, hostnames) without changing directory. Use when the user runs /ws-resume <branch> or asks to resume or work on a branch's worktree.
---

The user wants to work on a `ws` worktree, named by its **branch** — the argument (`$ARGS`).

## 1. Locate it and confirm it exists

The worktree lives at `.worktrees/<branch>/<repo>/`, where `<branch>` has slashes
replaced by dashes (`misc/test-ws` → `misc-test-ws`).

Run `ws list` and find the branch. **If it does not appear, stop: tell the user you
can't find a worktree for that branch.** Do not create it and do not run anything
else. You may mention `ws new <repo> <branch>` in case they meant to create it, but
leave the decision to them.

## 2. You are now operating on this worktree

For the rest of the session this branch is your working context. Operate it
**without `cd`-ing into it**:

- **Edit files** under `.worktrees/<branch>/<repo>/` (the path `ws list` printed).
- **Run commands** with `ws exec --branch <branch> <repo> -- <command>`. This runs
  where the stack lives (locally, or on the server for a remote workspace) with the
  right injected env. Do **not** run `podman compose` / build tools directly in the
  worktree — they miss the injected `COMPOSE_PROJECT_NAME` / `COMPOSE_FILE` and won't
  see the stack.
- **Status and lifecycle**: `ws list` (status), `ws up <branch>`, `ws down <branch>`.
  Do **not** auto-run `ws up`. If the stack is down, say so and let the user start it.
- **Reach HTTP services** at `<branch>.<service>.<domain>` (the domain is in
  `.ws.json`). Raw-TCP / host-process ports are forwarded to `localhost` — see
  `.ws-meta/<branch>-<repo>.json`.

## 3. Report back

Briefly tell the user what you found: the repos in this worktree, whether each is up
or down, and the hostname(s) — so it's obvious the resume landed and in what state.
