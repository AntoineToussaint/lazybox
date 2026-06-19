# Daemon, deployment & build

How lazybox is wired as a process, run, authenticated, configured, and built. The
client/daemon split underpins remote use and non-terminal clients; the JSON API
and structured runtime are the foundations for Tauri/iOS clients.

See [`DESIGN.md` § Architecture / IPC](../../DESIGN.md) and
[`ROADMAP.md`](../../ROADMAP.md) for the longer arc.

---

## Client/daemon split

**Status:** stable
**Crate(s):** `server`, `ipc` (`src/channel.rs`, `src/socket.rs`), `tui`
**Config / flags:** —
**Key bindings:** —

### What it does
The server owns all state and IO (PTYs, polling, the store); the TUI is a thin
renderer. By default they're the **same process**, talking over an in-memory
channel with zero serialization. Only when actually remote does traffic
serialize over a Unix socket.

### How to use it
Just run `lazybox` — the in-process daemon is the default. The split only becomes
visible when you run a [standalone daemon](#standalone-daemon) or
[connect remotely](#remote-connect).

### How it works (brief)
In-process: `run_embedded_realm` (`crates/tui/src/main.rs`) spawns the daemon on
a tokio task; transport is an mpsc channel pair (`crates/ipc/src/channel.rs`).
Out-of-process: a Unix socket with length-prefixed bincode frames (`u32` BE
length + payload, 64 MiB cap) in `crates/ipc/src/socket.rs`. The TUI code
doesn't branch on local vs remote — both sit behind the same transport trait.

### Test checklist
- [ ] `lazybox` with no flags runs the in-process daemon and works end-to-end.
- [ ] In-process mode does no socket IO (channel transport).
- [ ] Commands/events round-trip identically over channel and socket transports.

### Known sharp edges
- The 64 MiB frame cap bounds a single IPC message; pathologically large terminal bursts are chunked, not single-framed.

---

## Standalone daemon

**Status:** stable
**Crate(s):** `tui` (`main.rs`), `server`
**Config / flags:** socket at `~/.lazybox/v2/run/daemon.sock`, PID at `~/.lazybox/v2/run/daemon.pid`
**Key bindings:** —

### What it does
Runs the daemon as a long-lived background process, surviving client
disconnects — for SSH / multi-client setups (same model as a tmux server).

### How to use it
```sh
lazybox server start     # start a standalone daemon
lazybox server status    # PID + socket path if running
lazybox server stop      # SIGTERM the daemon
```

### How it works (brief)
`server_start` (`crates/tui/src/main.rs`) forks a daemon listening on the Unix
socket and writes a PID file. `server stop` sends SIGTERM via the lifecycle
helper; `server status` reports PID + socket. Socket/PID paths derive from
`LAZYBOX_HOME`.

### Test checklist
- [ ] `lazybox server start` starts a daemon and binds the socket.
- [ ] `lazybox server status` reports the running PID + socket path.
- [ ] `lazybox server stop` terminates it and clears the PID file.
- [ ] The daemon survives a client disconnect.

### Known sharp edges
- Single socket per `LAZYBOX_HOME`; a second `server start` against the same home contends for the socket.

---

## Remote connect

**Status:** beta
**Crate(s):** `ipc` (`src/socket.rs`), `tui`
**Config / flags:** `--connect <socket>`
**Key bindings:** —

### What it does
Connects a local TUI to a remote daemon over a Unix socket, typically forwarded
through SSH (`ssh -L`) — daemon on a beefy box, TUI on your laptop.

### How to use it
On the remote host run `lazybox server start`. Forward its socket over SSH, then:

```sh
lazybox --connect /path/to/forwarded.sock
```

### How it works (brief)
`run_remote` (`crates/tui/src/main.rs`) connects to the socket instead of
starting a local daemon; framing is the same length-prefixed bincode. SSH is the
trust boundary — there's no TCP/TLS in v2.0.

### Test checklist
- [ ] `lazybox --connect <socket>` attaches to a running daemon without starting a local one.
- [ ] Terminal replay reconstructs the screen on connect (ring-buffer replay).
- [ ] Disconnecting the client leaves the daemon (and its sessions) running.

### Known sharp edges
- No built-in transport security — relies entirely on SSH for the tunnel.
- Multi-user/multi-principal scoping is not implemented (ROADMAP §6); a shared daemon currently uses the daemon process's own provider credentials.

---

## JSON HTTP API gateway

**Status:** experimental
**Crate(s):** `server` (`api_gateway.rs`)
**Config / flags:** `LAZYBOX_API_ADDR` (default `127.0.0.1:8787`), `LAZYBOX_API_TOKEN` (required bearer unless `--insecure-no-auth`)
**Key bindings:** —

### What it does
Exposes the daemon over HTTP with newline-delimited JSON, so non-Rust clients
(Tauri, iOS) can list workspaces, stream events, and drive structured agent runs
without the terminal protocol.

### How to use it
```sh
LAZYBOX_API_TOKEN=secret lazybox server api   # bind 127.0.0.1:8787
LAZYBOX_API_TOKEN=secret lazybox server api 0.0.0.0:9000  # explicit addr
lazybox server api --insecure-no-auth         # explicitly unauthenticated
```

Without `LAZYBOX_API_TOKEN` the gateway refuses to start unless
`--insecure-no-auth` is passed.

Endpoints: `GET /v1/health`, `GET /v1/metrics` (event-pipeline drop/lag
counters), `GET /v1/workspaces`, `GET /v1/events` (NDJSON stream), `POST
/v1/commands` (single command), `POST /v1/stream` (duplex commands ↔ events).

### How it works (brief)
`server_api` (`crates/tui/src/main.rs`) parses the addr (arg → `LAZYBOX_API_ADDR`
→ default) and `LAZYBOX_API_TOKEN`, refusing to start without a token unless
`--insecure-no-auth` is passed. The gateway (`api_gateway.rs`)
serves the endpoints; streaming uses NDJSON frames (`JsonClientFrame::Command`
/ `JsonServerFrame::Event`). When a token is set, requests need
`Authorization: Bearer <token>`.

### Test checklist
- [ ] `GET /v1/health` returns OK.
- [ ] `GET /v1/metrics` returns the event-pipeline drop/lag counters as JSON.
- [ ] `GET /v1/workspaces` lists current workspaces as JSON.
- [ ] `GET /v1/events` streams NDJSON events.
- [ ] `POST /v1/commands` accepts a single command frame.
- [ ] `POST /v1/stream` round-trips commands → events.
- [ ] With `LAZYBOX_API_TOKEN` set, unauthenticated requests are rejected.
- [ ] Without `LAZYBOX_API_TOKEN` and without `--insecure-no-auth`, the
      command refuses to start.

### Known sharp edges
- Localhost-only by default and no CORS (ROADMAP §5); bearer auth is required at the CLI unless `--insecure-no-auth` is passed.
- No OpenAPI schema yet; the wire shapes are defined in `crates/ipc`.

---

## Run modes / flags

**Status:** stable
**Crate(s):** `tui` (`main.rs`), `core` (`src/paths.rs`)
**Config / flags:** `--fresh`, `--test`, `--connect`, `--workspace`, `--session`, `LAZYBOX_HOME`
**Key bindings:** —

### What it does
CLI flags and env to control startup: wipe state, run an isolated test profile,
preselect UI, or point the whole profile at a different home directory for a
side-by-side dev instance.

### How to use it
```sh
lazybox --fresh                       # wipe state.db, re-run setup
lazybox --test                        # tempdir + seeded session, no GitHub, no disk writes
LAZYBOX_HOME=~/.lazybox-dev lazybox       # separate state/worktrees/socket
lazybox --workspace <key>             # preselect a workspace
lazybox --session <id>                # preselect a session
```

### How it works (brief)
Flags parsed in `crates/tui/src/main.rs`. `--fresh` wipes
`~/.lazybox/v2/state.db`; `--test` builds a throwaway tempdir repo + seeded
session with no polling/disk writes. `LAZYBOX_HOME` is resolved by
`core::paths::home()` (`crates/core/src/paths.rs`) and every path (state DB,
worktrees, daemon socket, config) derives from it.

### Test checklist
- [ ] `--fresh` clears `state.db` and re-runs the setup wizard.
- [ ] `--test` opens a seeded session without touching real state or GitHub.
- [ ] `LAZYBOX_HOME=~/.lazybox-dev` uses a fully separate profile (no shared state, separate socket).
- [ ] `--workspace`/`--session` preselect the UI on startup.

### Known sharp edges
- `--test` is for local smoke testing — it doesn't exercise real provider auth.

---

## Auth / credential chain

**Status:** stable
**Crate(s):** `auth` (`CredentialProvider` trait + chain), `gh-provider`, `linear-provider`, `slack-provider`
**Config / flags:** `GH_TOKEN` / `GITHUB_TOKEN` / `gh auth token`; `LINEAR_API_KEY`; `slack.bot_token`/`app_token` (or `SLACK_BOT_TOKEN`/`SLACK_APP_TOKEN`)
**Key bindings:** —

### What it does
Resolves provider credentials from an ordered chain so zero-config works: GitHub
falls through env vars to `gh auth token`; Linear and Slack read their env/config
tokens.

### How to use it
Run `gh auth login` and GitHub just works. Set `LINEAR_API_KEY` for Linear and
the Slack tokens for the mirror. No lazybox-specific credential setup.

### How it works (brief)
`CredentialProvider` (`crates/auth`) has `name()` + `async resolve(scope)`. The
GitHub chain is `GH_TOKEN` env → `GITHUB_TOKEN` env → `gh auth token` command
(`crates/gh-provider/src/lib.rs`). Each provider builds its own chain. To add an
auth source, implement the trait and add it to the chain in `crates/server/`
(see [`CLAUDE.md`](../../CLAUDE.md)).

### Test checklist
- [ ] With no `GH_TOKEN`/`GITHUB_TOKEN`, creds resolve from `gh auth token`.
- [ ] Setting `GH_TOKEN` takes precedence over `gh auth token`.
- [ ] `LINEAR_API_KEY` enables the Linear provider.
- [ ] Slack tokens from env or `slack:` config both work.
- [ ] A missing required credential surfaces a clear error, not a crash.

### Known sharp edges
- Credentials are resolved from the daemon **process** environment — single-user by design today (ROADMAP §6 covers per-principal credentials).

---

## Config reference

**Status:** stable
**Crate(s):** `config` (`src/lib.rs`, `src/snippets.rs`)
**Config / flags:** `~/.lazybox/config.yaml` (rooted at `LAZYBOX_HOME`)
**Key bindings:** `,` opens the editor palette

### What it does
A single YAML file with sensible defaults for every option, so an empty file is
valid. Most of it is written by the setup wizard; the rest is hand-editable.

### How to use it
Top-level keys (`crates/config/src/lib.rs`):

| Key | Purpose |
|---|---|
| `setup` | Wizard output: enabled `providers`, `agents`, `filters`, `scopes`, `default_agent` |
| `editors` | Custom/override editor entries (`e`) — see [editor integration](workspaces-and-worktrees.md#editor-integration) |
| `repos.<owner/name>` | Per-repo `env` / `mounts` / `scripts` — see [per-repo overrides](workspaces-and-worktrees.md#per-repo-overrides) |
| `worktree` | Global `mounts`, `scripts`, `auto_cleanup_merged` |
| `providers` | Per-provider settings (e.g. `github.poll_interval`) |
| `slack` | `bot_token`, `app_token`, `anchor_channel`, `channel_prefix`, `per_workspace_channels` |
| `agent` | `autonomous_skip_permissions`, `skip_permissions`, nested agent config |
| `agent_shortcuts` | Single-char keys → agent ids |
| `attention` | Which signals flag a row: `unread`, `ci_failing`, `review_pending`, `agent_asking`, `mentioned`, `desktop_notify` |
| `ui` | View/behavior: `auto_mark_delay`, `quit_double_tap_window`, `terminal_escape_char`, `split_step_percent`, `task_body_max_rows`, `short_snooze`, `long_snooze`, `action_keys`, `tour_seen` |
| `display`, `shell`, `hooks`, `terminal`, `mention`, `auto_fix` | Display merging, shell, agent hooks, terminal, mention routing, auto-fix triggers |

### How it works (brief)
`Config::load()` reads `~/.lazybox/config.yaml` and fills missing fields from
`UiDefaults`/section defaults. Snippets are a separate file
(`~/.lazybox/snippets.yaml` + repo-local) loaded by `Snippets::load_merged`.

### Test checklist
- [ ] An empty `config.yaml` loads with all defaults (zero-config first run).
- [ ] A `ui.terminal_escape_char` override changes the terminal escape key.
- [ ] `providers.github.poll_interval` changes poll cadence within one interval.
- [ ] `attention.*` toggles change which rows get flagged.
- [ ] Editing config via the `,` palette writes valid YAML that re-loads.

### Known sharp edges
- `DESIGN.md` mentions a `lazybox config dump` command for the effective merged config — verify whether it's wired before relying on it.

---

## Build & install

**Status:** stable (build) / scaffolded-not-active (release channels)
**Crate(s):** build scripts, `libghostty-vt*` (vendored), `.github/workflows/release.yml`
**Config / flags:** `LAZYBOX_ZIG_CACHE`, `GHOSTTY_SOURCE_DIR`
**Key bindings:** —

### What it does
Builds lazybox from source with a pinned Zig toolchain and vendored ghostty VT
bindings. Release channels (Homebrew tap, curl installer, GitHub Releases via
cargo-dist) are wired but not yet activated (no `v*.*.*` tag pushed pre-1.0).

### How to use it
```sh
make setup   # download pinned zig 0.15.2 to ~/.cache/lazybox/zig/
make run     # build + run
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Prereqs: Rust 1.85+, a C compiler (bundled SQLite), `gh` for credentials, and
network on first build (ghostty Zig sources fetched at a pinned commit). Linux
also needs libc++ / libc++abi. Direct `cargo build` needs **zig 0.15.2** on
PATH.

### How it works (brief)
`make setup` caches zig host-wide (`~/.cache/lazybox/zig/<host>/`, override
`LAZYBOX_ZIG_CACHE`) so clones/worktrees share one download. The `libghostty-vt*`
Rust bindings are vendored; the underlying ghostty Zig sources are fetched at
build time (pinned commit, 3× retry; override with `GHOSTTY_SOURCE_DIR`). The
cargo-dist pipeline (`.github/workflows/release.yml` + `[workspace.metadata.dist]`)
builds macOS + Linux binaries on a version tag.

### Test checklist
- [ ] `make setup` downloads/caches zig 0.15.2.
- [ ] `make run` builds and launches from a clean checkout.
- [ ] A second worktree reuses the cached zig (no re-download).
- [ ] `GHOSTTY_SOURCE_DIR` lets a hand-cloned ghostty bypass the network fetch.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.

### Known sharp edges
- First build needs network for the ghostty source clone (HTTP/2 cancels happen; the script retries 3×).
- Release channels are scaffolded only — no tag pushed yet, so `brew`/curl installs aren't live.
- Windows is a non-goal; Linux gets less testing than macOS.
