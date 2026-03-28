# codiff

Realtime local diff review for AI-assisted coding.

`codiff` watches a project directory, computes live unified diffs, and serves a local web UI so you can review changes as they happen.

## Why codiff

- Review AI edits in realtime without constantly running `git diff`
- See changed files and per-file unified diffs in one local dashboard
- Attach instantly to the current folder with `codiff`
- Stay local-first: read-only watcher, no cloud dependency

## Features

- Realtime filesystem watcher
- Changed-files list with `added` / `modified` / `deleted` status
- Live unified diff view
- Startup baseline snapshot for session-based review
- Event coalescing (`--debounce-ms`) for bursty edit streams
- File safety limits (`--max-file-bytes`) for large/binary files
- Loopback-safe defaults for local security
- Single-instance lock per project (prevents duplicate launches in same repo)
- Global state directory for lock/session metadata
- SQLite-backed project/session history (`state.db`)
- File-level comments per session with resolve/reopen support
- Diesel-powered DB layer with migration files under `migrations/`

## Install

Download a prebuilt binary from GitHub Releases (recommended):

1. Open the latest release on GitHub
2. Download the archive for your platform
3. Unpack and place `codiff` somewhere on your `PATH`

Example for Linux x86_64:

```bash
tar -xzf codiff-linux-x86_64.tar.gz
sudo mv codiff /usr/local/bin/codiff
```

Optional (developers): build from source with Cargo.

## Quick start

Run from the project you want to review:

```bash
codiff
```

`codiff` starts the local server and opens your browser automatically to the correct URL.

Or pass a target path:

```bash
codiff /path/to/project
```

Open `http://127.0.0.1:8787`.

## CLI options

- `PATH` project directory to watch (default: `.`)
- `-p, --port <PORT>` HTTP server port (default: `8787`)
- `-H, --host <HOST>` bind host (default: `127.0.0.1`)
- `--allow-remote` allow non-loopback bind (required for `0.0.0.0`)
- `--max-file-bytes <N>` max file size to snapshot/diff (default: `500000`)
- `--debounce-ms <N>` watcher debounce window in ms (default: `150`)
- `--state-dir <PATH>` override global state directory (default: `~/.local/share/codiff`)
- `--api-token <TOKEN>` fixed API token (otherwise random token generated per run)
- `--max-concurrent-commands <N>` max concurrent AI command executions (default: `2`)
- `--command-timeout-secs <N>` max command runtime before forced failure (default: `180`)
- `--no-open-browser` disable automatic browser launch

## Examples

```bash
# default local review for current directory
codiff

# choose a different repo
codiff ../my-other-project

# use a different port
codiff -p 9000

# intentionally expose on LAN (explicit opt-in)
codiff . --allow-remote -H 0.0.0.0 -p 8787
```

## Security notes

- By default, codiff binds to `127.0.0.1` only.
- Non-loopback bind is blocked unless `--allow-remote` is set.
- Diffs may include sensitive file content in your project; use remote mode carefully.
- API and event endpoints require the per-run token (`x-codiff-token` or `?token=`).

## Runtime behavior

- `codiff` allows only one running instance per project path.
- Starting a second instance in the same repo exits with an error and prints the existing instance URL.
- A new session is created on each launch and stored in SQLite under the global state directory.
- Changed Files panel supports recency heat and optional `Recent only (60s)` filtering.
- Press `R` to jump to the most recently changed visible file.

## Session data

- Default DB path: `~/.local/share/codiff/state.db`
- Lock files: `~/.local/share/codiff/locks/`
- API: `GET /api/sessions` returns the latest sessions for the current project.

## Database layout

- DB access code: `src/db/`
- Diesel SQL migrations: `migrations/`
- API controllers: `src/controller/`
- Service layer (AI execution, orchestration): `src/service/`

## Current limitations

- Baseline is session-start snapshot (not yet git-HEAD mode)
- Ignore configuration is currently basic (`.git/`, `target/`)
- Single-process local runtime (no multi-session project registry yet)

## Development

```bash
cargo check
cargo run --
```

## Release binaries

- GitHub Actions workflow at `.github/workflows/release.yml` builds platform binaries on tag push (`v*`).
- Example: pushing tag `v0.1.0` publishes downloadable release assets.
