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
| `cargo nextest run --workspace` | Run all tests with the repository's per-test timeout policy |
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
| `lazybox --connect [socket]` | Connect to a remote daemon over a Unix socket; with no path, defaults to `~/.lazybox/run/daemon.sock` |
| `lazybox --workspace <key>` | Preselect a workspace on startup |
| `lazybox --session <id>` | Preselect a session on startup |
| `lazybox scan [ROOTS...] [--depth N] [--hidden]` | Find existing git clones and linked worktrees without modifying them |
| `lazybox hook-ingest --backend-key <key>` | Internal: forward an agent lifecycle hook payload (stdin JSON) to the daemon — lazybox injects this into spawned agents' hook config; not typically run by hand |

## `lazybox scan`

`lazybox scan` is a read-only inventory of git checkouts created outside
lazybox. Pass one or more roots directly, or configure `scan.roots` and run it
with no positional arguments:

```bash
lazybox scan ~/code ~/work --depth 5
lazybox scan --hidden
```

| Option | Effect |
| --- | --- |
| `[ROOTS...]` | Directories to walk; overrides `scan.roots` when present |
| `--depth N` | Maximum levels below each root; overrides `scan.max_depth` |
| `--hidden` | Include dot-directories, which are skipped by default |

Results are ordered by recent commit activity and identify the branch, path,
linked worktrees, dirty checkouts, and any checkout lazybox already tracks.
Lazybox's own managed worktree directory is excluded. The command is read-only —
it only reports what it finds. To import a discovered checkout in place, use
the in-app import flow: press `x i` inside lazybox to scan and link a checkout
as a workspace without moving it.

## `lazybox server`

| Command | What it does |
| --- | --- |
| `lazybox server start` | Start the daemon in the **foreground** (blocks until shutdown — run it in tmux, `nohup`, or a service unit) |
| `lazybox server stop` | Stop the daemon |
| `lazybox server status` | Report whether the daemon is running |
| `lazybox server api [addr:port] [--insecure-no-auth] [--allow-insecure-http]` | Start the JSON HTTP API gateway |

### `lazybox server api` auth

The API gateway **refuses to start without an auth decision**: either set
`LAZYBOX_API_TOKEN` (clients then send `Authorization: Bearer <token>`), or
pass `--insecure-no-auth` to explicitly serve unauthenticated (a warning is
printed).

The listen address resolves in order: the `[addr:port]` argument, then the
`LAZYBOX_API_ADDR` environment variable, then the default `127.0.0.1:8787`.
The gateway has no built-in TLS. A non-loopback bind is refused unless
`--allow-insecure-http` is also supplied; use that acknowledgement only behind
an authenticated TLS proxy, SSH tunnel, or trusted private overlay network.

`POST /v1/commands` waits for the command handler to finish before returning
`{"ok":true,"completed":true}`. Provider/domain outcomes still arrive as
normal events. Streaming requests are connection- and command-bounded, so a
client must reconnect after the documented stream limit instead of growing an
unbounded daemon queue. `GET /v1/workspaces` includes a `warnings` array when
an unreadable row was preserved and omitted from the decoded workspace list.

## `lazybox slack`

| Command | What it does |
| --- | --- |
| `lazybox slack init` | Set up the Slack mirror from your config |
| `lazybox slack doctor` | Diagnose token, scope, and connectivity issues |
| `lazybox slack prune` | **Archive** stale per-workspace channels (Slack has no channel delete) |

### `lazybox slack prune` options

The command computes a plan, prints it, and prompts before archiving anything.

| Option | Effect |
| --- | --- |
| `--dry-run` | List what would be archived without touching Slack |
| `--yes`, `-y` | Skip the confirmation prompt |
| `--older-than DUR` | Only archive channels stale for at least this long (e.g. `7d`) |
| `--workspace KEY` | Restrict pruning to one workspace's channels |

## Environment variables

| Variable | Effect |
| --- | --- |
| `GH_TOKEN`, `GITHUB_TOKEN` | GitHub credential (otherwise `gh auth token` is used) |
| `LINEAR_API_KEY` | Credential for the Linear provider |
| `RUST_LOG` | Log filter, e.g. `RUST_LOG=lazybox=debug` for verbose logs |
| `LAZYBOX_HOME` | Overrides every path lazybox writes under `~/.lazybox`: state, config, worktrees, runtime dir, tmux socket. Logs are separate — they default to `/tmp/lazybox.log` (override with `ui.log_path`) |
| `LAZYBOX_RUNTIME_DIR` | Overrides just the daemon runtime directory (`daemon.sock` / `daemon.pid`); wins over `LAZYBOX_HOME`'s default `<home>/run/` |
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
| `~/.cache/lazybox/ghostty/` | Pinned Ghostty source and Zig package cache used by offline builds |
