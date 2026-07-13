---
title: CLI reference
description: Every lazybox subcommand, flag, and run mode.
---

All commands assume a `lazybox` binary on your `PATH` — installed via Homebrew
(`brew install AntoineToussaint/lazybox/lazybox`), the `curl | sh` installer, or
a source build (see the [Quickstart](/docs/tutorials/quickstart/)). From a
source checkout without a built binary on `PATH`, substitute
`cargo run -p lazybox-tui --`.

An `lb` binary ships alongside `lazybox` — a short alias with the identical
entrypoint, so every command below works as `lb …` too.

## Make targets

For day-to-day development, the `Makefile` wraps the common flows.

| Target | What it does |
| --- | --- |
| `make setup` | One-shot: download pinned Zig 0.15.2 to `~/.cache/lazybox/zig/` |
| `make run` | Build and run lazybox |
| `make build` | Build the workspace |
| `make release` | Build in release mode |
| `make test` | Run the test suite |
| `make lint` | Run clippy |
| `make fmt` | Format the workspace |
| `make dev` | Run a side-by-side instance against `LAZYBOX_HOME=~/.lazybox-dev` |
| `make run-fresh` | Run with state wiped and the setup wizard re-run |
| `make run-test` | Run the seeded throwaway instance |

## Direct cargo

| Command | What it does |
| --- | --- |
| `cargo build` | Build |
| `cargo run -p lazybox-tui` | Build and run the TUI binary |
| `cargo test --workspace` | Run all tests |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint, warnings as errors |

If you build with cargo directly (no `make`), put Zig 0.15.2 on your `PATH`
first.

## `lazybox` run modes

| Command | What it does |
| --- | --- |
| `lazybox` | Default — in-process daemon plus TUI |
| `lazybox --help`, `-h` | Print the command overview and exit |
| `lazybox --version`, `-V` | Print the version and exit |
| `lazybox --fresh` | Wipe `~/.lazybox/v2/state.db` and re-run the setup wizard |
| `lazybox --test` | Throwaway tempdir repo with one seeded workspace, no GitHub |
| `lazybox --connect <socket>` | Connect to a remote daemon over a Unix socket |

## `lazybox server`

| Command | What it does |
| --- | --- |
| `lazybox server start` | Start the daemon |
| `lazybox server stop` | Stop the daemon |
| `lazybox server status` | Report whether the daemon is running |
| `lazybox server api [addr:port] [--insecure-no-auth]` | Start the JSON HTTP API gateway |

### `lazybox server api` auth

The API gateway **refuses to start without an auth decision**: either set
`LAZYBOX_API_TOKEN` (clients then send `Authorization: Bearer <token>`), or
pass `--insecure-no-auth` to explicitly serve unauthenticated (a warning is
printed).

The listen address resolves in order: the `[addr:port]` argument, then the
`LAZYBOX_API_ADDR` environment variable, then the default `127.0.0.1:8787`.

## `lazybox slack`

| Command | What it does |
| --- | --- |
| `lazybox slack init` | Set up the Slack mirror from your config |
| `lazybox slack doctor` | Diagnose token, scope, and connectivity issues |
| `lazybox slack prune` | Remove stale per-workspace channels |

## Environment variables

| Variable | Effect |
| --- | --- |
| `GH_TOKEN`, `GITHUB_TOKEN` | GitHub credential (otherwise `gh auth token` is used) |
| `LINEAR_API_KEY` | Credential for the Linear provider |
| `RUST_LOG` | Log filter, e.g. `RUST_LOG=lazybox=debug` for verbose logs |
| `LAZYBOX_HOME` | Overrides every path lazybox writes: state, worktrees, tmux socket |
| `LAZYBOX_API_TOKEN` | Bearer token for `lazybox server api` (required unless `--insecure-no-auth`) |
| `LAZYBOX_API_ADDR` | Listen address for `lazybox server api` when no `[addr:port]` argument is given |

## Paths

| Path | Contents |
| --- | --- |
| `~/.lazybox/v2/state.db` | Persistent state (read/unread, snooze, sessions) |
| `~/.lazybox/config.yaml` | Configuration (see [Configuration](/docs/reference/configuration/)) |
| `~/.lazybox/run/daemon.sock` | Daemon Unix socket (`lazybox server start` / `--connect`) |
| `~/.lazybox/snippets.yaml` | Global snippet library (repo-local: `<repo>/.lazybox/snippets.yaml`) |
| `/tmp/lazybox.log` | Logs (override with `ui.log_path`) |
| `~/.cache/lazybox/zig/` | Pinned Zig toolchain from `make setup` |
