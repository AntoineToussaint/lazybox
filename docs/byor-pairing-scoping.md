# BYOR easy pairing — relay + QR + per-device creds — scoping (#749)

_The **product layer** on top of the bring-your-own-remote plumbing. [#742][issue-742]
makes a client and a daemon *able* to talk remotely (the SSH / manual-TLS MVP).
This issue is what would make that **easy and sellable**: a thin relay, QR
pairing, per-device credentials, and an entitlement gate — the foundation a
future iOS client would ride. Part of the BYOR lane under the managed-daemon
epic [#728][issue-728]; reuses the [#648][issue-648] licensing/entitlement work.
This page sizes the work and names the decisions; it is not a product or pricing
commitment._

## Where this sits

Two neighbouring scoping notes bound this one:

- [remote-daemon-scoping.md][remote] (#728) established that the **self-hosted**
  "run the daemon on my box, connect from my laptop" story is architecturally
  real today, and that the cost of turning it into a *product* is the control
  plane (accounts, billing, brokering), not the transport.
- [session-transfer-scoping.md][transfer] (#729) established that the sellable
  "start on laptop, finish on iPhone" shape is **attach/detach to a persistent
  daemon**, not literal session transfer — a thin client over the daemon the
  laptop already left running.

This note is the connective tissue between them: **how a client reaches that
persistent daemon without the user configuring SSH, DNS, TLS, or port
forwarding** — and how the easy path is gated on a subscription. The heavy
multi-tenant *hosting* tier stays out of scope (that's [#728][issue-728] via
mind); BYOR keeps the daemon on the **user's own box**, which is exactly why the
sharp execution-sandbox problem does not return here.

## Why a relay (not SSH, not direct TLS)

The [#742][issue-742] MVP works but is not "easy," and on iOS it is closer to
impossible:

- **SSH `-L`** needs a keypair, a reachable host/port, and a shell — see the
  self-hosted runbook in [remote-daemon-scoping.md][remote]. No iOS client can
  forward a Unix socket over `ssh -L`.
- **Direct TLS** needs the user to own certs, DNS, and an inbound port on a
  self-hosted box behind NAT. That is cert-management work most users will not
  do.

The relay model (Tailscale / VS Code Tunnels / Plex-style) removes all of it:

- The **box dials *out*** to a rendezvous service and holds the connection open.
  Works behind NAT, no inbound ports, no DNS, no certs on the box.
- Clients reach the box **through** the relay. With **end-to-end encryption
  (box ↔ client)** the relay forwards **ciphertext only** — it never sees
  GitHub tokens, prompts, or terminal output.
- The relay is therefore also the **payment-enforcement point**: it brokers a
  connection only for accounts with an active subscription. The thing that makes
  the easy path easy is the thing we control, so the gate is robust rather than
  a client-side honor system.

Open-core falls out cleanly: gate the *easy* path (relay + QR), leave the raw
SSH / direct-TLS path from [#742][issue-742] ungated for the self-hosting 1%.
Trying to gate SSH would be both futile and off-brand.

## What already exists to build on

The relay layers pairing and brokering onto transport that is already here — it
does not invent a new protocol:

- **Framed wire protocol.** `Command`/`Event` are serialized as length-prefixed
  bincode frames (`[u32 BE length][bincode payload]`) over any tokio
  `AsyncRead`/`AsyncWrite` pair (`crates/ipc/src/socket.rs:258`). Today that pair
  is a Unix socket; a relay-tunnelled byte stream is the same pair with a
  different carrier. The handshake exchanges a 4-byte magic marker + wire
  fingerprint and a build descriptor — and **nothing else**: no authentication,
  no encryption
  (`write_preamble` / `read_build`, `crates/ipc/src/socket.rs:109`,`:137`).
- **Remote attach client.** `lazybox --connect <socket>` already runs a full TUI
  against a daemon it did not start (`run_remote`,
  `crates/tui-boot/src/main.rs:700`; dispatch at `:360`), with ring-buffer
  replay reconstructing the screen on connect.
- **Non-terminal client surface.** The JSON HTTP gateway plus **structured agent
  runs** (Claude launched `-p --input-format stream-json --output-format
  stream-json`, `crates/agents/src/agent.rs:65`) exist specifically so a
  phone/Tauri client can drive an agent without a PTY. This is the right shape
  for the iOS follow-on (see [session-transfer-scoping.md][transfer]).

So a relay-brokered client is "the existing `--connect` client, over a tunnel,
after a pairing handshake." The three genuinely-new pieces are below.

## What is genuinely new

### 1. Relay + registration + E2E channel

- **Box side (`lazybox serve`):** generate a **persistent identity keypair**
  stored on the box, authenticate once to the account (login / device-code flow
  against the control plane), and register with the relay under that account —
  then hold the outbound connection open.
- **Relay service (codefly-hosted):** a dumb encrypted-byte forwarder. Box
  registration, client brokering by box id, and — critically — an **entitlement
  check before it will broker** (§3). It executes nothing and, under E2E, sees
  only ciphertext.
- **E2E key exchange:** an authenticated handshake (recommend **Noise / X25519**)
  wrapping the existing framed byte stream, so the relay carries ciphertext
  end to end. This is a **new layer *under* the current fingerprint handshake**,
  not a replacement: once the encrypted channel is up, the existing
  `Command`/`Event` framing and fingerprint check run inside it unchanged.

There is no crypto, Noise, or relay-client dependency in the tree today — this
is net-new surface, most of it a codefly service plus a client-side channel
crate; the daemon change is comparatively small (dial-out registration + wrap
the socket).

### 2. QR pairing + per-device credentials

1. The box shows a QR encoding **{relay URL, box public key, one-time
   short-TTL pairing code}**.
2. A client scans → connects via the relay → runs the authenticated key
   exchange. The **box public key in the QR pins the box's identity**, so a
   compromised or malicious relay cannot MITM; the **one-time, short-TTL code**
   means a leaked QR screenshot is not a standing risk.
3. The box mints a **per-device credential**, stored in the OS keystore (iOS /
   macOS Keychain). Every later connect is E2E-encrypted with that credential.
4. **Revocation:** per-device creds are individually revocable from the box /
   account, so a lost phone revokes exactly one device.

Keep two credential notions distinct, because it is easy to conflate them:

- **Per-device credential** = the *client's* E2E identity for reaching the box
  through the relay. New, per this issue, revocable per device.
- **Provider tokens** (GitHub / Linear / Slack) stay on the box and resolve from
  the daemon **process** environment via the single credential chain
  (`crates/auth/src/chain.rs:11`). That single-principal model is a documented
  limitation for a *shared* daemon
  ([remote-daemon-scoping.md][remote], hardening gaps) — but for **BYOR it is
  correct, not a gap**: one trusted operator, their creds, their box. Per-device
  pairing authenticates *devices of that one operator*, not multiple principals,
  so it deliberately does **not** try to solve per-connection provider-credential
  scoping.

### 3. Entitlement gate

- **Enforcement (server-side):** the relay refuses to broker for an account
  without an active subscription. This is the durable gate — no subscription, no
  relay, no easy connect.
- **UX (client-side):** the client also carries an entitlement flag reusing the
  [#648][issue-648] licensing work — the "Upgrade to connect remotely" prompt.
  That flag is only the UX affordance; it is **not** the enforcement (a patched
  client still gets refused by the relay). Note the entitlement *client* is
  itself still forward work in the [#648][issue-648] estimate
  ([desktop-spike.md][desktop], packaging/licensing boundary) — there is no
  licensing code in-tree yet — so this gate lands *after* #648 ships an
  entitlement primitive to reuse.

### 4. iOS client (follow-on, its own issue)

A thin native client that scans the QR, renders the inbox from the client-free
view-model ([#731][issue-731] / [#732][issue-732]) and a VT emulator, and
connects through the relay. Foundation only here — flagged as out of scope by the
issue itself and best tracked separately.

## Build vs. adopt (the load-bearing open decision)

The issue's own recommendation is right: **evaluate an existing substrate before
building a relay.** The E2E-through-a-blind-relay pattern is well-trodden, and a
hand-rolled Noise-over-a-custom-forwarder is a lot of security-sensitive surface
to own. Candidates to weigh against a minimal custom relay:

- **Tailscale / WireGuard** — mature NAT traversal and E2E, but a heavy client
  dependency and its own identity/ACL model to reconcile with codefly accounts;
  the entitlement gate would have to hook its coordination layer.
- **A QUIC relay / libp2p circuit relay** — closer to "dumb encrypted
  forwarder," lighter client, but more assembly (identity, pairing, brokering)
  left to us.
- **An off-the-shelf E2E channel** (a Noise implementation such as `snow`) over a
  minimal custom rendezvous — most control over the pairing UX and the
  entitlement hook, most crypto surface to own.

The decision hinges on where the **entitlement check** and **per-account box
registration** compose most cleanly, since that gate is the commercial point of
the whole exercise. A substrate whose brokering we cannot condition on a
subscription check does not serve the business goal even if its transport is
excellent.

## Codefly / saas-starter dependency

Codefly hosts the **thin control plane** — it does **not** run anyone's daemon:

- **Relay / rendezvous** — the blind encrypted-byte forwarder, box registration,
  client brokering.
- **Accounts + billing + entitlement** — precisely `codefly-dev/module-saas-starter`'s
  remit.
- **Pairing API** — issue one-time pairing codes, tie devices to accounts, and
  **check entitlement before brokering**.

The margin shape is the pitch: *the user hosts the heavy compute (their box); we
host the thin brokering + billing.* The open operational question is where the
relay lives in codefly and how the pairing API composes with saas-starter's
account model — that belongs in a codefly-side design, not this repo.

## Security model

- **Relay is blind.** E2E encryption means provider tokens, prompts, and terminal
  output never cross the box↔client channel in cleartext. Contrast the current
  JSON gateway, which is loopback-only *precisely because* bearer-over-plaintext
  is neither transport encryption nor principal isolation (`ensure_loopback`
  refuses any non-loopback bind, `crates/server/src/api_gateway.rs:630`,`:139`).
  The relay path is what earns a routable listener: E2E crypto replaces "must be
  loopback / must tunnel over SSH."
- **Box identity pinned** by the public key carried in the QR — a malicious relay
  cannot MITM.
- **Pairing codes** one-time and short-TTL — a leaked screenshot is not a
  standing risk.
- **Per-device creds** revocable individually, in the OS keystore.
- **Execution trust is unchanged and correct for BYOR.** The daemon still runs
  agents unsandboxed as the user on the user's own box — a single trusted
  operator, their creds, their machine. The relay executes nothing. The
  multi-tenant execution-sandbox problem (and its release gates in the
  [monetization strategy][monetization], "Cloud security and isolation plan")
  returns only for the deferred *hosted* SaaS tier ([#728][issue-728] via mind),
  **not** here.

## Recommendation

1. **Sequence behind its dependencies.** This is a product layer; it needs
   [#742][issue-742]'s transport to exist first, and it reuses [#648][issue-648]'s
   entitlement primitive — which is itself still forward work. Land those before
   the relay, or the gate has nothing to check and the tunnel nothing to wrap.
2. **Decide build-vs-adopt before writing a line of relay.** The crypto and NAT
   traversal are a large, security-sensitive surface; prefer an existing
   substrate unless its brokering cannot be conditioned on the entitlement check.
   Prototype the **entitlement-gated brokering** against a candidate substrate as
   the first spike — it is the load-bearing risk and the commercial point.
3. **Keep the layers honest.** The E2E channel is a new layer *under* the
   existing fingerprint handshake and framed `Command`/`Event` protocol; the
   relay stays a blind forwarder; the daemon change is dial-out registration plus
   wrapping the socket. Resist letting the relay grow smarts — its blindness is
   both the security property and the reason it is cheap to run.
4. **Treat the iOS client as a separate issue.** Foundation only here; the app is
   its own effort riding the client-free view-model ([#731][issue-731] /
   [#732][issue-732]) and the structured-agent-run stream.

**Anchors:** framed wire + no-auth handshake (`crates/ipc/src/socket.rs:258`,`:109`);
remote attach (`run_remote`, `crates/tui-boot/src/main.rs:700`; `--connect`,
`:360`); structured agent runs (`crates/agents/src/agent.rs:65`); single-principal
credential chain (`crates/auth/src/chain.rs:11`); loopback-only gateway
(`ensure_loopback`, `crates/server/src/api_gateway.rs:630`). Related scoping:
[remote-daemon-scoping.md][remote] (#728), [session-transfer-scoping.md][transfer]
(#729), [desktop-spike.md][desktop] / [#648][issue-648] (licensing boundary),
[monetization strategy][monetization] (isolation gates for the deferred hosted
tier). Depends on [#742][issue-742] and `codefly-dev/module-saas-starter`.

[issue-742]: https://github.com/AntoineToussaint/lazybox/issues/742
[issue-728]: https://github.com/AntoineToussaint/lazybox/issues/728
[issue-648]: https://github.com/AntoineToussaint/lazybox/issues/648
[issue-731]: https://github.com/AntoineToussaint/lazybox/issues/731
[issue-732]: https://github.com/AntoineToussaint/lazybox/issues/732
[remote]: remote-daemon-scoping.md
[transfer]: session-transfer-scoping.md
[desktop]: desktop-spike.md
[monetization]: monetization-strategy.md
