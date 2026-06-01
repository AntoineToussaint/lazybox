# pilot

A reactive PR inbox in your terminal.

Instead of refreshing GitHub, events flow to you — new comments, CI
failures, review requests surface as they land. Each task becomes a
session with an embedded terminal for running Claude Code, Codex,
Cursor, or a shell in a git worktree.

Source-agnostic: GitHub today, Linear tomorrow, Jira after that.
Same UI, same key bindings, same inbox.

## Status

Pre-1.0, **early-adopter dev mode**. Daily-driver for the author on
macOS; Linux runs the same code paths but gets less testing.
Expect sharp edges, log spam in `/tmp/pilot.log`, and the
occasional breaking change. Shareable with technical friends who
are happy to file bugs.

Run the side-by-side dev profile (`make dev`) if you want to try
pilot without disturbing your main inbox.

## Quick start (dev)

```sh
git clone https://github.com/AntoineToussaint/pilot.git
cd pilot
make setup           # one-shot: download pinned zig 0.15.2 to ~/.cache/pilot/zig/
make run             # build + run
```

Or for repeated dev launches:

```sh
make run                       # default
make run ARGS="--fresh"        # any flag via ARGS=
make run-fresh                 # shortcut for the common cases
make run-test
make run-connect SOCKET=/tmp/pilot.sock

./run.sh --fresh               # the bash flavor — same effect
```

**Side-by-side dev profile** — run a second pilot instance against
its own state DB + tmux socket without disturbing your daily-driver
copy (handy while iterating on pilot itself):

```sh
make dev          # PILOT_HOME=~/.pilot-dev pilot
make dev-fresh    # wipe ~/.pilot-dev first
```

`PILOT_HOME` overrides every path pilot writes — state, worktrees,
tmux socket — so the two instances share zero state.

**What `make setup` does:**
- Downloads zig 0.15.2 into a host-level cache (`~/.cache/pilot/zig/<host>/`,
  override with `PILOT_ZIG_CACHE`). Caching it outside the checkout
  means every clone and git worktree shares one download instead of
  each re-fetching ~45MB. The Rust bindings are vendored under
  `crates/libghostty-vt*/`, but the underlying ghostty Zig sources are
  fetched at build time from github.com/ghostty-org/ghostty (pinned
  commit).
- Verifies `cargo` and `gh` are on PATH.

**Prerequisites:** Rust 1.85+, a C compiler (for bundled SQLite),
the GitHub CLI for credentials (`gh auth login`), and network
access to github.com on first build (subsequent builds use the
cached `target/.../ghostty-src/`).

**Linux** also needs libc++ + libc++abi (Zig builds ghostty
against LLVM's C++ stdlib, not GNU libstdc++):

```sh
# Debian / Ubuntu
sudo apt-get install -y build-essential pkg-config libc++-dev libc++abi-dev

# Fedora / RHEL
sudo dnf install -y gcc-c++ pkgconf-pkg-config libcxx-devel libcxxabi-devel

# Arch
sudo pacman -S --needed base-devel pkgconf libc++ libc++abi
```

Without these, the ghostty static archive links cleanly but
linking pilot fails with `undefined reference to std::__1::*`.

If you `cargo build` directly (no Makefile / `run.sh`), put
**zig 0.15.2** on PATH first — newer zig trips ghostty's
`requireZig` check.

**Network hiccups during the first build?** GitHub clones over
HTTP/2 occasionally cancel mid-stream. The build script retries
3× automatically; if all three fail, clone the ghostty source by
hand and point at it:

```sh
git clone --filter=blob:none https://github.com/ghostty-org/ghostty.git /tmp/ghostty-src
cd /tmp/ghostty-src && git checkout a1e75daef8b64426dbca551c6e41b1fbc2b7ae24
GHOSTTY_SOURCE_DIR=/tmp/ghostty-src cargo build
```

## Install (release)

The `cargo-dist` release pipeline is wired (see
`.github/workflows/release.yml` + `[workspace.metadata.dist]`).
It builds prebuilt binaries for macOS (aarch64 + x86_64) and Linux
(aarch64 + x86_64) and publishes:

- **Homebrew tap** — `brew install <user>/pilot/pilot`
- **Curl install script** — `curl -sSf https://.../install.sh | sh`
- **GitHub Releases** — binary tarballs for manual download

Released by pushing a `v*.*.*` tag. Pre-1.0, no tag has been pushed
yet — these channels are scaffolded and ready, not yet active.

## Run

```sh
pilot                       # default — in-process daemon + TUI
pilot --fresh               # wipe ~/.pilot/v2/state.db, re-run setup
pilot --test                # tempdir + seeded session, no GitHub
pilot --connect <socket>    # connect to a remote daemon over a Unix socket
pilot server start          # standalone daemon (for SSH / multi-client)
pilot server api [addr:port]   # JSON HTTP API gateway
```

`gh auth token` provides GitHub creds by default. `LINEAR_API_KEY`
provides Linear creds. Run with `RUST_LOG=pilot=debug` for verbose
logs, which go to `/tmp/pilot.log` (the TUI takes the screen).

## Key bindings

**Global** (anywhere except inside a terminal):

| Key | Action |
|---|---|
| `Tab` | Cycle Sidebar → Activity → Terminals (also escapes Terminals on first press before any input) |
| `?` | Help overlay |
| `,` | Settings palette (add repo / edit roles / pick agents / …) |
| `q q` | Quit (double-tap within 800ms) |
| `!` | Jump to next workspace whose agent is waiting on input |
| `Ctrl-Shift-D` | Detach focused pane into a new pilot window |
| `Shift-arrows` | Resize splitters |
| `F8` / `Alt-s` | Toggle pilot's mouse capture — flip OFF for host-native trackpad selection (whole-screen), flip back ON for pilot's pane-scoped selection / splitter drag |

**Sidebar** (workspace list):

| Key | Action |
|---|---|
| `j/k` | Navigate |
| `Enter` | Open the focused workspace |
| `Space` | Fold / unfold the parent repo group |
| `s` | Spawn a shell |
| `c` | Spawn Claude Code |
| `x` | Spawn Codex |
| `u` | Spawn Cursor |
| `w` | Work — spawns Claude with the right prompt for the row's state (fix CI / fix conflict / address comments / implement issue) |
| `f` | Cycle role filter: `all → author → reviewer → assignee → mentioned`. Chip on row 1 of the sidebar header; click to cycle too |
| `o` | Cycle sort order: `recent → by-role → split` (split adds role-section headers between groups within each repo). Click the chip to cycle |
| `m` | Mark **all** of this workspace's activity as read (bulk) |
| `n` | New pre-PR workspace |
| `e` | Open the worktree in your editor (Zed / VS Code / Cursor / …) |
| `Shift-R` | Manual refresh — re-poll providers right now |
| `Shift-M` | Merge PR (opens a Confirm modal; arrows / Tab navigate Yes/No) |
| `Shift-X` | Archive workspace (Confirm modal; kills running sessions if any) |

**Activity** (right pane):

| Key | Action |
|---|---|
| `j/k` / `↑/↓` | Navigate the activity feed (scroll follows the cursor) |
| Mouse wheel | Scroll the activity list (8 rows/notch) |
| `PageUp/PageDown` | Jump by a screenful |
| `g/G` | Top / bottom |
| `h/l` (or `←/→`) | Collapse / expand the focused comment |
| `v` | Toggle multi-select on the focused row (`w` / reply target the set) |
| Click row | Toggle multi-select + move cursor (footer hint confirms count) |
| Double-click row | Toggle expand/collapse on that card |
| Click `▶/▼ Description` | Toggle the description section |
| Click `▸/▾ Activity` | Toggle the activity section |
| `m` | Mark the focused comment as read |
| `z` | Undo the most recent auto-mark-read |
| `b` | Toggle the Description section |
| `Enter/Space/o` | Collapse / expand the whole Activity section |

**Cross-pane** (works from Sidebar OR Activity):

| Key | Action |
|---|---|
| `r` | Reply (open the textarea targeted at the selected workspace) |

**Terminals:** all keys forward to the PTY. `Ctrl-c` is SIGINT.
`]]` (two presses) returns to the sidebar. `Tab` cycles focus when
you haven't typed anything yet in the current visit (so a fresh
visit doesn't trap you); after the first keystroke `Tab` routes
to the PTY for autocomplete. Mouse wheel scrolls tmux's scrollback
or forwards to the inner program if it has mouse-tracking on
(Claude Code, vim, less). Left-click + drag does pane-scoped text
selection — release copies to the clipboard via OSC 52 (footer
shows `copied N lines`). For the host's native selection (spans
across pilot's UI but works in every terminal), press `F8` to
flip mouse capture off, drag, then `F8` to flip back.

**Pickers** (Settings palette, scope/agent/repo pickers — any
`Choice` modal):

| Key | Action |
|---|---|
| `j/k` or `↑/↓` | Navigate |
| `Space` | Toggle (multi-select pickers) |
| `Enter` | Confirm |
| `PageUp/PageDown` | Jump a screen at a time |
| `Ctrl-u/Ctrl-d` | Half-page jump (vim-style) |
| `Home/End` or `g/G` | First / last item |
| `Backspace` | Back to the previous step (where applicable) |
| `Esc` or `Ctrl-c` | Cancel |

## Architecture

16 pilot crates in a client/daemon split (+ 2 vendored libghostty
crates for the embedded terminal). The four core libraries (`core`,
`auth`, `events`, `store`) never depend on each other. Providers
plug in via two traits — see
[`crates/core/src/provider.rs`](crates/core/src/provider.rs) and
[`crates/core/src/scope.rs`](crates/core/src/scope.rs).

```
crates/
  core/            # Task, Session, Activity. Source-agnostic.
  auth/            # CredentialProvider trait + chain.
  events/          # In-process event bus.
  store/           # Store trait + SQLite backend.
  config/          # YAML loader.
  git-ops/         # Worktree manager.
  tui-term/        # Embedded terminal (libghostty-vt + portable-pty).
  gh-provider/     # GitHub PRs + Issues.
  linear-provider/ # Linear issues.
  slack-provider/  # Slack mirror (per-workspace channels, Socket Mode).
  ipc/             # Wire types + transport (in-process channel + Unix socket).
  agents/          # Agent trait + Claude/Codex/Cursor/Generic builtins.
  llm-proxy/       # 127.0.0.1 HTTP pass-through that records token usage.
  server/          # Daemon: PTY lifecycle, polling, JSON API gateway.
  tui-core/        # Pure UI primitives (Action catalog, key chords) shared with the TUI.
  tui/             # Realm-based TUI client. Hosts the `pilot` binary.
```

See [`CLAUDE.md`](CLAUDE.md) for the long-form architecture notes
and [`DESIGN.md`](DESIGN.md) for the rationale behind the
client/daemon split + the pane / modal / component tiers.

## Configuration

Per-user config lives at `~/.pilot/config.yaml`. The setup wizard
(`,` from inside pilot, or first-launch) writes most of it; press
`,` any time to add a repo / change agents / edit roles without
nuking state.

**Editors** (the `e` shortcut): pilot detects Zed / VS Code /
Cursor / Windsurf / Fleet / IDEA / Gram at startup. Add custom
entries or override builtins via `editors:`:

```yaml
editors:
  - id: zed
    display: "Zed (insider)"
    command: /Applications/Zed-Insiders.app/Contents/MacOS/zed
    args: ["{path}"]
  - id: my-editor
    command: /opt/myeditor/bin/edit
    args: ["--workspace", "{path}"]
```

`{path}` is replaced with the workspace's worktree dir at launch.

**Per-repo overrides** (`repos:`): inject env vars and symlink
shared folders into worktrees on a per-repo basis. Useful when
different projects need different `DATABASE_URL`s, or when you
want to share vendored code across worktrees without committing
it. Keyed by `owner/name` as GitHub reports it.

```yaml
repos:
  tensorzero/tensorzero:
    env:
      DATABASE_URL: postgres://localhost/dev
      OPENAI_API_KEY: sk-...
    mounts:
      - source: ~/shared/tensor-data
        link_at: _imports/data
      - source: ~/code/vendored/foo
        link_at: _imports/foo
    scripts:
      - name: cleanup
        source: ~/dev/scripts/rust-cleanup.sh
      - name: setup
        content: |
          #!/usr/bin/env bash
          cargo fetch
```

`env` is injected into every shell / agent PTY pilot spawns inside
that repo's worktrees (added on top of the daemon's process env;
per-repo wins on key collision). `mounts` symlinks are applied
after `git worktree add` and stack on top of the global
`worktree.mounts` list. `placement: inside` (default) puts the
link inside the worktree; `placement: above` puts it in the
worktree's parent dir.

`scripts` materializes executable files under
`<worktree>/_pilot/scripts/<name>` after checkout. Pick one of
`content` (inline body, shebang auto-injected if missing) or
`source` (path to an existing script, symlinked through so edits
on the source flow through without re-running the spawn). Both
stack on top of `worktree.scripts`. Useful for per-repo cleanup
(`cargo clean`, prune target/), setup, or any project-specific
tool you want callable inside every worktree.

A ready-to-use Rust workspace cleanup script lives at
`scripts/examples/rust-cleanup.sh` in this repo. Copy it somewhere
on your machine and reference it from `repos.<owner/name>.scripts`
to get `./_pilot/scripts/cleanup` inside every worktree.

**Slack mirror** (optional): pilot can mirror workspace activity
into per-workspace Slack channels and route `@pilot`-mentioned
replies back into the running agent's stdin — useful for keeping an
eye on long-running Claude/Codex sessions from your phone. Bot
token (`xoxb-...`) and Socket Mode app token (`xapp-...`) live
under `slack:`; see [`docs/slack-setup.md`](docs/slack-setup.md)
for the Slack app manifest and [`docs/dev/slack-testing.md`](docs/dev/slack-testing.md)
for the smoke test.

```yaml
slack:
  bot_token: xoxb-...
  app_token: xapp-...
  anchor_channel: pilot
  per_workspace_channels: true
```

**Autonomous sessions** (`agent:`): when pilot picks up `@pilot`-
triggered work it spawns the agent unattended — no human is sitting
there to answer tool-use approval prompts. So those sessions launch
Claude Code with `--dangerously-skip-permissions` ("no-permission"
mode), and the tab strip flags them with a `⚠ no-perms` badge so it's
obvious which sessions aren't gated by approvals. Sessions you open
yourself (`c` in the sidebar, `f`-for-fix) keep prompts on by default —
the approval *is* the human-in-the-loop guard. Flip the autonomous
default off to force prompts on every autonomous session, or opt your
own interactive sessions into bypass mode (also toggleable from
Settings → "Skip permission prompts for your sessions"):

```yaml
agent:
  autonomous_skip_permissions: false  # prompt even autonomous runs
  skip_permissions: true              # bypass on sessions you open too
```

The bypass works under both Claude subscription login (Pro/Max) and an
Anthropic API key; the only restriction is that Claude Code refuses to
start in bypass mode as root/sudo, which the worktree sessions don't
hit. The blast radius stays bounded to the per-task git worktree, and
the [autonomous work-prompt principles](crates/core/src/prompts.rs)
(no destructive shortcuts, no `--no-verify`, root-cause over masking)
are the in-prompt counterweight to the relaxed approvals.

State (workspace activity, read/unread, snooze, terminal scrollback
ring) persists in `~/.pilot/v2/state.db` via SQLite.

## Contributing

Issues and PRs welcome. Two ground rules:

- **Tests with every change.** Library crates have unit tests; UI
  components have ratatui `TestBackend` snapshots; the orchestrator
  has integration tests in `crates/tui/tests/`.
- **No new dependencies in the four core libraries** without
  discussion — the layering is what keeps the codebase honest.

## License

MIT — see [`LICENSE`](LICENSE).
