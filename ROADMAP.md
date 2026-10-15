# Roadmap

Where `drup` is heading. No dates — items land when they're needed and someone
shows up to do them. In-flight bug fixes and cleanups live in `notes/`, not
here.

---

## Linux client support

macOS is the only supported client today. The offload server is already Linux,
so the underlying mechanisms are cross-platform; what's missing:

- Surface the `Platform::Linux` paths currently gated off in `src/setup.rs`.
- Add `x86_64-unknown-linux-gnu` back to the release matrix in
  `.github/workflows/release.yml` — the matrix shape was kept for exactly this.
- Exercise the routing and DNS pieces that only run on the macOS client today.

---

## Multiple remote servers per workspace

A workspace pins to one remote today (or none). Once several developers share
an offload server, one machine fills up while the others sit idle. The intended
shape:

- `.drup.json` declares a named list of remotes and marks one default.
- `drup up` uses the default; `drup up --remote=<name>` picks a specific one.
- `.drup-meta/` records the landing remote per task, so every subsequent
  `drup exec` / `drup down` / `drup here` routes to the right server without guessing.
- `drup remotes` (name TBD) lists what's configured so a human can choose
  informed.

Explicitly **not**: capacity-based auto-fallback, round-robin, or in-flight
retry. Capacity is fuzzy before a stack is up, and hiding the routing choice
inside the tool makes commands surprising in ways that are hard to debug. The
`--remote=` selector is a human primitive; devs eyeball their own load.

Open question: what happens when `drup up --remote=X <task>` runs against a task
already up on Y. Likely answer is refuse and require `drup down` first;
cross-server migration is a separate rabbit hole.
