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

## Install

```bash
cargo install --path .
```

## Quick start

Run from the project you want to review:

```bash
codiff
```

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

## Current limitations

- Baseline is session-start snapshot (not yet git-HEAD mode)
- Ignore configuration is currently basic (`.git/`, `target/`)
- Single-process local runtime (no multi-session project registry yet)

## Development

```bash
cargo check
cargo run --
```
