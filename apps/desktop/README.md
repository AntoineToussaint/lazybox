# lazybox desktop client

This focused Tauri v2 client consumes the production desktop boundary:

- the Rust shell starts `lazybox-server` through the shared `ClientRuntime`;
- an authenticated gateway binds an ephemeral loopback port;
- the frontend imports Rust-generated, versioned contract types;
- inbox/control events use NDJSON while terminal data uses bounded raw binary
  frames suitable for xterm.js;
- reconnect uses daemon snapshots, sequence metadata, replay, and resync.

The bearer token and gateway URL stay in Rust. The webview reaches them only
through narrow Tauri commands and never receives a reusable credential.

## Run

Complete lazybox setup once so `~/.lazybox/config.yaml` and the state database
exist, then:

```sh
cd apps/desktop
npm ci
npm run tauri dev
```

Review the frontend without a daemon:

```sh
npm run dev
# open http://localhost:1420/?preview
```

Preview data is compiled out of production builds.

## Verify

```sh
npm test
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
```

See [`../../docs/desktop-spike.md`](../../docs/desktop-spike.md) for the
lifecycle, protocol, security, and separate-product packaging boundary.
