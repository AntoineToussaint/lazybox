# lazybox desktop spike

This is a deliberately narrow Tauri v2 client proving that the existing
client/daemon boundary can support a commercial desktop app:

- the Rust process starts the production `lazybox-server` services in-process;
- an authenticated gateway binds to an ephemeral loopback port;
- the webview lists the real persisted inbox and follows live NDJSON events;
- `Spawn`, `Write`, `Resize`, replay, sequence-gap detection, and terminal
  resync drive one interactive xterm.js terminal.

The bearer token stays in Rust. The webview reaches the gateway through typed
Tauri commands and an ordered Tauri channel, so the prototype does not add a
permissive CORS policy or expose the token to JavaScript.

## Run

Complete lazybox setup once so `~/.lazybox/config.yaml` and the state database
exist, then:

```sh
cd apps/desktop
npm install
npm run tauri dev
```

The frontend can be reviewed without starting the daemon:

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

See [`../../docs/desktop-spike.md`](../../docs/desktop-spike.md) for the API
gap analysis, delivery estimate, and licensing recommendation.
