# workspaces

`ws` — a Rust CLI for git-worktree orchestration across multi-repo workspaces, with a plugin-style hook system for project-specific behavior.

## Build

```
cargo build --release
cargo nextest run 
```

Prereq for tests: `cargo install cargo-nextest --locked` (one-time, per machine).
