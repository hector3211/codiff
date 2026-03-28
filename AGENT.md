# codiff Agent Guide

## Mission

Build `codiff` into a fast local companion that makes AI edits reviewable in realtime:

- watch local file changes as they happen
- group and stream updates with low latency
- provide a browser UI to inspect changed files and unified diffs
- stay read-only by default so review never interferes with editing

## Product Scope

Current implementation target (MVP):

1. Run a local server and file watcher in one binary
2. Capture changes relative to session baseline snapshot
3. Push updates to the browser in realtime
4. Render changed-files list and per-file unified diff

Planned next:

- git `HEAD` baseline mode
- change-batch timeline
- search/filter and hunk jump shortcuts
- optional patch export and session replay

Recently completed UX improvements:

- highlighted recency in Changed Files list (hot/warm/stale)
- optional recent-only filter (last 60 seconds)
- keyboard shortcut `R` to jump to latest visible change

## Architecture

- `src/main.rs`: CLI, watcher setup, diff generation, SSE streaming, API routes
- `src/db/`: Diesel-based DB access and session/comment persistence
- `web/index.html`: local UI (no build step)
- `migrations/`: SQL schema migrations managed by Diesel
- baseline snapshot: in-memory map of `path -> file contents`
- live state: in-memory map of changed files and latest diff payload
- session state: SQLite DB in global state dir (`state.db`)

## Development Rules

- Optimize for local-first developer workflow
- Avoid heavy runtime dependencies unless they unlock core UX
- Keep UI framework-free until requirements outgrow vanilla JS
- Handle binary/large files safely (skip or summarize)
- Respect ignores: `.git/`, `target/`, and user-defined patterns later
- Keep host binding loopback-only by default; require explicit opt-in for remote access
- Enforce one active codiff instance per project path
- Persist project/session metadata in local SQLite

## Done Criteria (for each increment)

- App compiles with `cargo check`
- Watch loop detects create/modify/delete quickly
- UI updates without manual refresh
- Diff view clearly shows before/after content
- README-level run instructions remain accurate

## How to run

```bash
cargo run --
```

After install (`cargo install --path .`), run from any project directory with just:

```bash
codiff
```

Useful flags:

- `--max-file-bytes 500000` to cap snapshot/diff file size
- `--debounce-ms 150` to batch bursty edits
- `--allow-remote` only when you intentionally want non-loopback bind
- `--state-dir <PATH>` to override global state/lock directory

Open `http://127.0.0.1:8787` and edit files under the watched path.
