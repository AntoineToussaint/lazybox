# lazybox desktop client

The private macOS client covers the focused daily lazybox workflow:

- first-run GitHub authentication, repository scope, and default-agent setup;
- inbox search, attention filters, unread state, task metadata, and activity;
- reviewed GitHub replies plus visible success and failure state;
- workspace-backed agent and shell sessions with terminal input, resize, replay,
  and reconnect recovery;
- shared YAML settings for provider scope and agent choice;
- keyboard navigation, accessibility semantics, local crash diagnostics, and
  opt-in content-free analytics.

The Rust shell starts `lazybox-server` through the shared `ClientRuntime` and
binds an authenticated gateway to an ephemeral loopback port. The bearer token,
gateway URL, GitHub credentials, and agent credentials stay in Rust. The
webview receives only repository metadata and the versioned desktop contract.

## Run from a clean checkout

On macOS with the Rust toolchain, Node 22, and Xcode command-line tools:

```sh
cd apps/desktop
npm ci
npm test
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --locked
npm run tauri build -- --debug --bundles app
```

The app bundle is written to:

```text
target/debug/bundle/macos/lazybox.app
```

CI packages that bundle as `lazybox-macos-dogfood.tar.gz` before artifact
upload so the executable mode inside the `.app` survives download. Extract the
archive with `tar -xzf lazybox-macos-dogfood.tar.gz`.

Open that app and complete setup in the first-run dialog. GitHub CLI
browser-based login stores its credential in the existing `gh`/OS credential
store; lazybox never asks the user to paste a token into the webview.

For development:

```sh
npm run tauri dev
```

Review the frontend without a daemon or live provider credentials:

```sh
npm run dev
# open http://localhost:1420/?preview
```

The preview data exercises the interface without a daemon. The native
credential-free integration test exercises persisted setup, the authenticated
gateway, inbox mutation, and a real shell PTY without a GitHub credential.

## Privacy and diagnostics

Analytics is off by default. When enabled, the native boundary accepts a
closed enum of event names and timestamps; it has no field capable of carrying
provider or terminal content. The private build records those events locally
at `~/.lazybox/v2/desktop-analytics.ndjson`.

Crash diagnostics are always local and contain the build version, platform,
architecture, and panic location only. The Settings dialog shows their exact
directory, normally `~/.lazybox/v2/desktop-crashes`.

See [`../../docs/desktop-spike.md`](../../docs/desktop-spike.md) for the
lifecycle, protocol, security, and separate-product packaging boundary.
