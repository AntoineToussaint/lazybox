# Desktop client boundary

The desktop client consumes the same public daemon as the TUI without copying
daemon startup or Rust wire definitions. Product code is limited to the Tauri
adapter and web UI; provider, store, worktree, PTY, polling, recovery, and agent
behavior remain in the MIT-licensed Rust crates.

## Embedded lifecycle

`lazybox_server::client_runtime::ClientRuntime` owns the reusable service
lifecycle:

- backend-session recovery with a bounded startup wait;
- optional persisted-session restore;
- legacy sandbox migration;
- provider polling, keep-awake, and scheduled agent updates;
- optional Slack startup;
- graceful task shutdown.

The TUI, standalone daemon, foreground API, and desktop shell construct this
runtime from one `ServerConfig`. Client-owned behavior stays outside it: the
TUI setup wizard and embedded Unix socket, the standalone socket service, and
the desktop loopback HTTP adapter. The standalone daemon continues to leave
persisted-session restore to its durable backend, while embedded clients and
the foreground API request restore.

## Versioned contract

`GET /v1/protocol` reports the desktop protocol version, Rust wire fingerprint,
daemon build, binary terminal media type, and frame/write limits. Desktop
requests send `x-lazybox-protocol-version`; an unsupported version receives
HTTP 426 with the requested and supported values.

The version is the compatibility gate. The fingerprint over-approximates the
wire contract — a `Cargo.lock` bump or a comment edit flips it — so across a
remote-daemon hop (#815) two independently-built binaries routinely disagree on
it while speaking the same wire. It is therefore advisory: the client compares
the daemon's `/v1/protocol` fingerprint with its own and, on a mismatch under a
compatible version, surfaces a "these builds differ, update one" notice instead
of aborting the connection.

TypeScript definitions under `apps/desktop/src/generated/` are generated from
the Rust desktop command/event DTOs, core model, and gateway DTOs:

```sh
cargo run -p lazybox-server --features desktop-contract \
  --bin generate-desktop-contract
UPDATE_DESKTOP_CONTRACT=1 cargo test -p lazybox-server --test api_gateway \
  desktop_compatibility_fixture_is_current -- --exact
```

The committed compatibility fixture is serialized from every desktop command
and event shape. Frontend tests pin the protocol version, fingerprint, and
variant coverage. The desktop CI job regenerates the types and fails on any
diff, so a Rust desktop wire change cannot silently leave the frontend
contract stale. `make desktop-contract` runs both steps above with the pinned
zig toolchain.

Because the fingerprint is hashed over `crates/{ipc,core}/src` + `Cargo.lock`,
*any* edit under those crates rewrites `apps/desktop/src/generated/*` — so a
branch rebased across such an edit conflicts on the generated files every time.
`make rebase-main` automates that away: it rebases onto `origin/main` and, for a
conflict confined to the generated contract, regenerates it from the merged tree
(which git has already checked out at the conflict stop) instead of leaving you
to hand-regenerate. Any conflict outside the generated dir stops the rebase for
manual resolution — the tool only ever automates the mechanical regenerate step,
never a real code merge.

## Terminal transport

Control and lifecycle events remain authenticated NDJSON. Terminal byte
payloads are removed from that stream and use `POST /v1/terminal` with media
type `application/vnd.lazybox.terminal.v1`.

Server frames are length-prefixed:

```text
u32 body length (big endian)
u8  kind: snapshot=1, output=2, resync=3, scrollback=4, resync-unavailable=5
u64 terminal id
u64 first sequence
u64 last sequence
raw terminal bytes
```

Client frames use a length, kind, terminal id, and command payload. Kinds are
write, resize, resync, close, and fetch scrollback. JSON command routes reject
those terminal commands so all terminal traffic shares the ordered binary
queue. Frames and writes are capped by the existing IPC limits. Each gateway
bridge retains the daemon's bounded
drop-and-authoritative-replay behavior; the Tauri shell adds a bounded native
queue and the webview pulls raw chunks, so neither side accumulates an
unbounded terminal backlog. xterm.js receives `Uint8Array` payloads directly,
not JSON number arrays.

## Security

The embedded gateway binds an ephemeral IPv4 loopback port with a random
per-process bearer. The bearer is stored only in Rust: the webview invokes
narrow Tauri commands and never receives the gateway URL or credential. The
public `server api` command is also loopback-only. Remote use must tunnel the
loopback listener over an encrypted channel such as SSH.

Direct remote HTTP remains disabled because bearer authentication is not
transport encryption or principal isolation. Enabling a routable listener
requires a future design with TLS and principal-scoped authorization; a flag
cannot waive that boundary.

## Delivery status and estimate

The reusable desktop boundary shipped in
[#666](https://github.com/AntoineToussaint/lazybox/pull/666) and is complete.
Starting from that merged baseline, the remaining path to a paid macOS release
is intentionally split into two product milestones:

- [the private macOS MVP](https://github.com/AntoineToussaint/lazybox/issues/647)
  requires an estimated 2–3 engineer-weeks;
- [the paid macOS v1](https://github.com/AntoineToussaint/lazybox/issues/648)
  then adds an estimated 4–6 engineer-weeks for entitlements, distribution,
  updates, recovery, and support hardening.

These incremental estimates leave roughly 6–9 engineer-weeks for one senior
engineer familiar with Rust and TypeScript. The estimate assumes macOS first
and excludes Apple account approval delays, external legal review, full TUI
parity, hosted execution, and Windows or Linux support. Features beyond the
focused paid workflow should be ported only when MVP usage demonstrates
demand.

## Packaging and licensing boundary

The reusable engine can remain public and MIT licensed:

- `core`, `auth`, `store`, `config`, providers, `git-ops`, `agents`, and `ipc`;
- `server`, including `ClientRuntime`, the gateway, PTY ownership, and protocol
  generation;
- the TUI and public daemon binaries.

A separately distributed product can depend on those crates while keeping its
desktop UI, signing/package metadata, updater, entitlement client, and hosted
account services in a proprietary repository. It imports the generated
versioned contract and calls the shared lifecycle; it does not fork daemon boot
logic or hand-copy IPC shapes. Licensing, billing, updater behavior, and hosted
multi-tenancy are intentionally outside this repository's boundary.

## Distribution and updates (macOS)

The TUI ships through cargo-dist (`[workspace.metadata.dist]` in the root
`Cargo.toml`): a shell installer plus a Homebrew *formula* pushed to the
`AntoineToussaint/homebrew-lazybox` tap. cargo-dist builds one Rust binary; it
cannot build a Tauri app or emit a Homebrew cask, so the desktop app has its
own release path that runs off the *same* version tag.

**Decision — the desktop ships as a Homebrew cask.**

- `.github/workflows/release-desktop.yml` triggers on the same `v<x.y.z>` tags
  as `release.yml`. It builds a **universal** (`aarch64` + `x86_64`) macOS
  `.app` + `.dmg` from `apps/desktop`, **signs** it with a Developer ID cert
  and **notarizes + staples** it through Apple (Tauri v2 does both when the
  `APPLE_*` env is present), attaches the `.dmg` to the GitHub Release that
  cargo-dist created for that tag, and pushes a rendered
  `Casks/lazybox-desktop.rb` (template in
  `apps/desktop/packaging/lazybox-desktop.rb.tmpl`) to the same tap.
- Install: `brew install --cask lazybox-desktop`. **Update path:**
  `brew upgrade --cask lazybox-desktop` — the cask *is* the updater, so the
  desktop needs no in-app Tauri updater for the Homebrew channel (the TUI's
  build-guard modal remains TUI-only). A direct `.dmg` download from the
  release is the non-Homebrew fallback.
- The cask token `lazybox-desktop` is deliberately distinct from the CLI
  `lazybox` formula; both can be installed side by side.
- **Version coherence (#815):** the release workflow stamps the tag's version
  into `tauri.conf.json` and `package.json` before building, so the desktop and
  TUI release from one version and their `/v1/protocol` fingerprints line up.

**Enabling / required secrets.** The workflow is gated on the
`DESKTOP_RELEASE_ENABLED` repository variable so tagging never reds the desktop
build before signing is provisioned. To turn it on, add the Apple Developer
secrets (`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`; the
existing `HOMEBREW_TAP_TOKEN` is reused) and set `DESKTOP_RELEASE_ENABLED` to
`true`. `workflow_dispatch` builds + signs a chosen tag as a dry run without
attaching to a release or pushing the cask. The CI `desktop` job still builds
the *unsigned debug* dogfood bundle on every PR touching `apps/desktop`; the
signed universal bundle is release-only.

## Verification

```sh
cargo test -p lazybox-server --test api_gateway
cargo test -p lazybox-ipc --test protocol
cd apps/desktop
npm ci
npm test
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
```

The gateway integration suite uses a real raw PTY and covers binary input,
resize, sustained output, a stalled consumer, authoritative recovery,
disconnect, reconnect replay, and explicit resync.
