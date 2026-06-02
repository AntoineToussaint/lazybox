# CLI reference

All commands assume pilot has been built from source (see the
[Quickstart](../tutorials/quickstart.md)). Where this page writes `pilot`, you
can substitute `cargo run -p pilot-tui --` if you have not put a built binary on
your `PATH`.

!!! note "No prebuilt releases yet"
    `brew install` and `curl | sh` install paths are scaffolded but not active —
    no version has been tagged. Build from source until 1.0 ships.

## Make targets

For day-to-day development, the `Makefile` wraps the common flows.

| Target | What it does |
| --- | --- |
| `make setup` | One-shot: download pinned Zig 0.15.2 to `~/.cache/pilot/zig/` |
| `make run` | Build and run pilot |
| `make build` | Build the workspace |
| `make release` | Build in release mode |
| `make test` | Run the test suite |
| `make lint` | Run clippy |
| `make fmt` | Format the workspace |
| `make dev` | Run a side-by-side instance against `PILOT_HOME=~/.pilot-dev` |
| `make run-fresh` | Run with state wiped and the setup wizard re-run |
| `make run-test` | Run the seeded throwaway instance |

## Direct cargo

| Command | What it does |
| --- | --- |
| `cargo build` | Build |
| `cargo run -p pilot-tui` | Build and run the TUI binary |
| `cargo test --workspace` | Run all tests |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint, warnings as errors |

If you build with cargo directly (no `make`), put Zig 0.15.2 on your `PATH`
first.

## `pilot` run modes

| Command | What it does |
| --- | --- |
| `pilot` | Default — in-process daemon plus TUI |
| `pilot --fresh` | Wipe `~/.pilot/v2/state.db` and re-run the setup wizard |
| `pilot --test` | Throwaway tempdir repo with one seeded workspace, no GitHub |
| `pilot --connect <socket>` | Connect to a remote daemon over a Unix socket |

## `pilot server`

| Command | What it does |
| --- | --- |
| `pilot server start` | Start the daemon |
| `pilot server stop` | Stop the daemon |
| `pilot server status` | Report whether the daemon is running |
| `pilot server api [addr:port]` | Start the JSON HTTP API gateway (default `127.0.0.1:8787`) |

## `pilot slack`

| Command | What it does |
| --- | --- |
| `pilot slack init` | Set up the Slack mirror from your config |
| `pilot slack doctor` | Diagnose token, scope, and connectivity issues |
| `pilot slack prune` | Remove stale per-workspace channels |

## Environment variables

| Variable | Effect |
| --- | --- |
| `GH_TOKEN`, `GITHUB_TOKEN` | GitHub credential (otherwise `gh auth token` is used) |
| `LINEAR_API_KEY` | Credential for the Linear provider |
| `RUST_LOG` | Log filter, e.g. `RUST_LOG=pilot=debug` for verbose logs |
| `PILOT_HOME` | Overrides every path pilot writes: state, worktrees, tmux socket |

## Paths

| Path | Contents |
| --- | --- |
| `~/.pilot/v2/state.db` | Persistent state (read/unread, snooze, sessions) |
| `~/.pilot/config.yaml` | Configuration (see [Configuration](configuration.md)) |
| `/tmp/pilot.log` | Logs (override with `ui.log_path`) |
| `~/.cache/pilot/zig/` | Pinned Zig toolchain from `make setup` |
