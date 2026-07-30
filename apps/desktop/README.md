# lazybox macOS desktop

This focused Tauri v2 client supports the daily GitHub inbox-to-agent workflow:

- the Rust shell starts `lazybox-server` through the shared `ClientRuntime`;
- an authenticated gateway binds an ephemeral loopback port;
- the frontend imports Rust-generated, versioned contract types;
- first-run setup detects GitHub and installed agents, discovers repository
  scopes, and persists through the shared YAML model;
- the inbox supports search, unread/CI/review filters, activity, reply,
  mark-read, local workspace creation, agents, shells, and terminal recovery;
- inbox/control events use NDJSON while terminal data uses bounded raw binary
  frames suitable for xterm.js;
- reconnect uses daemon snapshots, sequence metadata, replay, and resync.

The bearer token and gateway URL stay in Rust. The webview reaches them only
through narrow Tauri commands and never receives a reusable credential. GitHub
authentication launches `gh auth login --web`; the app then resolves the same
`LAZYBOX_GITHUB_TOKEN` → `GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token` chain as
the daemon.

## Run

Install the GitHub CLI and at least one supported agent (`claude`, `codex`, or
`cursor-agent`), then:

```sh
cd apps/desktop
npm ci
npm run tauri dev
```

On first launch the app guides GitHub sign-in, repository selection, and
default-agent selection. It writes only the shared `setup:` and `desktop:`
sections of `~/.lazybox/config.yaml`, preserving all other settings, then
restarts its embedded daemon with that configuration.

Review the frontend without a daemon:

```sh
npm run dev
# open http://localhost:1420/?preview
```

Preview data is compiled out of production builds.

## Supported workflow

- `/` focuses inbox search; `↑` and `↓` move through filtered workspaces.
- `⌘R` refreshes providers and `⌘,` opens settings.
- Start or resume the selected agent, start or resume a shell, and reconnect to
  daemon-owned terminal replay after an app restart.
- Post replies through the daemon's existing provider mutation and error path.
- Create a named worktree under the selected repository, optionally starting
  the chosen agent immediately.
- Close a live terminal only after the desktop confirmation names the effect.

All controls have programmatic labels, dialogs have explicit cancel paths, and
status/error changes use live regions. Keyboard-only smoke coverage should
include first-run setup, inbox filtering, workspace selection, reply, settings,
agent/shell start, terminal input, and terminal close.

## Privacy and diagnostics

Anonymous analytics and crash diagnostics are separate opt-ins and default
off. The analytics command accepts five fixed enum values with no free-form
payload, so task content, repository names, credentials, prompts, replies, and
terminal output cannot enter it. This internal build has no remote analytics
exporter.

When crash diagnostics are enabled, the native panic hook appends only the app
version, Unix timestamp, and Rust source basename/line to
`~/.lazybox/v2/desktop-crash.log`. It does not record panic text or application
content.

## Internal macOS build

Build the `.app` from a clean checkout:

```sh
cd apps/desktop
npm ci
npm test
npm run tauri build -- --bundles app
open ../../target/release/bundle/macos/lazybox.app
```

The manually dispatched `Desktop dogfood build` workflow runs the frontend,
native tests, formatting, and clippy checks before uploading the unsigned
internal `.app` as a 14-day artifact. Signing, notarization, public update
channels, and entitlements remain part of the paid-release milestone.

## Verify

```sh
npm ci
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

See [`../../docs/desktop-spike.md`](../../docs/desktop-spike.md) for the
lifecycle, protocol, security, and separate-product packaging boundary.
