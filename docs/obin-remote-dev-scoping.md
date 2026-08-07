# Remote app dev on a GCE box — scoping (obin-platform first)

_Run a full application dev stack on a **per-user GCE box** that lazybox stamps
per worktree, and connect to it from anywhere — the TUI/desktop driving agents
**and** the app's own web UI in a browser. First target: **obin-platform**
(a multi-service Next.js + FastAPI + Python-agent stack). Tracked by epic #885._

This note sits on top of the existing remote plumbing — [`remote-daemon-scoping.md`][remote]
(#728), [`byo-remote-runbook.md`][runbook] (#742), and [`byor-pairing-scoping.md`][pairing]
(#749). Those established that the **self-hosted "daemon on my box, client on my
laptop"** story is architecturally real today, and that the sellable *link* is a
relay + QR. This note adds the missing halves for a real workload: **provisioning
the box**, **running an app stack on it**, and **carrying the app's web port**
back to the client without breaking its auth.

## Why this is different from #742/#749

#742/#749 move the **lazybox control plane** (the daemon: PTYs, agents,
worktrees) to a remote box. But a real dev loop also has a **data plane** — the
app under development, which for obin is a web app on `localhost:3000` behind a
third-party SSO (WorkOS). Two independent connection problems:

| Plane | What it is | Connect today | "Link from another computer" |
|---|---|---|---|
| **Control** | the lazybox daemon | `server start` + `ssh -L`/IAP + `--connect` — **works** (`socket.rs:452-690`) | relay + QR — **designed only** (#749) |
| **Data** | the obin web app `:3000` + WorkOS | `ssh -L 3000` → browser hits `localhost:3000` — **works** | see the WorkOS constraint below |

## The WorkOS constraint (and why localhost forwarding dissolves it)

The obin Portal authenticates users with **WorkOS AuthKit**. There is **no dev
bypass** — `DEV_MODE` only skips *service-to-service* GCP-ID-token checks, not
the browser login. After login, WorkOS redirects the browser to
`<origin>/callback`, and **WorkOS only permits redirect URIs on an allowlist
configured in its dashboard** (no API — a manual step; see obin-platform's
`create-dev-env` Phase H). `http://localhost:3000/callback` is already
allowlisted for local dev.

Therefore the whole design keeps the **browser on `localhost:3000`**:

- **`ssh -L`/IAP forward** (today): the connecting machine binds `localhost:3000`
  → WorkOS is happy, zero dashboard changes.
- **Relay path** (#894): the relay must **also multiplex the workload TCP port**
  (`:3000`, `:8082`) and bind it to `localhost` **on the far client**. That
  browser then sees `localhost:3000` too — so "log in from another computer"
  works with **no public host and no allowlist change, ever.**

A public tunnel URL (Tailscale/Cloudflare) would work but forces a WorkOS
dashboard allowlist entry per host, and exposes the app — so it is the fallback,
not the plan.

## Two facts that make this cheaper than it looks

1. **The wire is transport-agnostic.** `Command`/`Event` are length-prefixed
   bincode frames over any `AsyncRead`/`AsyncWrite` pair (`ipc/src/socket.rs:718`);
   the Unix socket is just today's carrier. A relay/tunnel byte stream drops in
   unchanged, and **reconnect + resync is already solved and tested**
   (`socket.rs:452-690`) — a flaky VM link inherits it for free.
2. **lazybox provisions the box, so it pins the box's build** — sidestepping the
   #815 protocol-skew hard-fail for our topology. (The `:3000` forward is a raw
   TCP proxy anyway, immune to protocol skew.)

## Target architecture

```
GCE VM in internal-robin-dev  (per-user, stamped per worktree from a golden image)
 ├─ attached SA -> ADC (Vertex, Firestore on platform-portal-dev, GCS skills, run.invoker on dev tools)
 ├─ lazybox daemon:  server start (socket)  +  server api (127.0.0.1:8787 gateway)   [systemd, #887]
 └─ obin stack:      tools/local-dev/dev up <profile>  ->  portal :3000, harness :8082, postgres, ...

Client (laptop or any computer with lazybox + gcloud):
   IAP/relay tunnel forwards BOTH:
     • daemon socket/gateway  -> lazybox --connect (TUI) / desktop
     • :3000, :8082           -> browser hits localhost:3000 -> WorkOS login works
```

## What already exists vs is net-new

**Reusable:** daemon/client split, framed wire, reconnect/resync, TUI remote
attach (`--connect`, `run_remote` at `main.rs:948`), the loopback JSON gateway,
structured agent runs for non-PTY clients.

**Net-new (nothing to reuse):** VM provisioning + daemon lifecycle on GCE; any
off-localhost transport (both transports are localhost-only by design —
`ensure_loopback`, `api_gateway.rs:1304`); per-device client identity
(`PrincipalId` is hardwired to `local`, `ipc/src/lib.rs:434`); and every piece of
the link/pairing story (relay, Noise E2E, QR, per-device creds, entitlement — all
docs-only in #749).

## Tracks (epic #885)

- **Track A — obin-box (the demo).** `#886`. Lives in the **obin-platform** repo
  (`experiments/antoinetoussaint/obin-gce-box/`): gcloud/Terraform for the VM +
  SA, a startup-script that installs the toolchain and brings up `dev up`, a
  `connect.sh` IAP wrapper, and a golden image. **This is the demoable-tomorrow
  slice** — no lazybox code required. Follow-ons: `#901` (golden image /
  instance template for fast per-user stamping) and `#902` (stop-on-idle +
  lifecycle / cost control).
- **Track B — lazybox ↔ box glue.** `#887` systemd daemon unit · `#888`
  remote-host targeting (a worktree's daemon lives on a provisioned box) · `#889`
  in-process tunnel supervisor (daemon + workload ports) · `#890` worktree
  post-create hook runs `dev up`.
- **Track C — relay + QR link (#749).** `#891` E2E channel (Noise/X25519) · `#892`
  per-device identity · `#893` rendezvous relay + `lazybox serve` · `#894` QR/link
  pairing + workload-port forwarding · `#895` entitlement gate (stub first).
- **Track D — desktop remote.** `#896` desktop connect-to-remote + protocol-skew
  (refs the closed #814/#815).

## Sequencing

1. **Track A** end-to-end → a remote obin box usable daily over IAP. *(demo)*
2. **Track B** → lazybox drives the box per worktree; TUI + web "just work."
3. **Track C** (`#891`→`#893`→`#894`→`#895`) → the productized link. `#891` and
   `#892` are independent and can start immediately, in parallel with A/B.
4. **Track D** whenever the desktop becomes the daily driver.

The interim "link from another computer" is a generated IAP/SSH **connection
bundle** (host + tunnel command + forwarded ports) — buildable in Track A,
WorkOS-clean — until the relay (`#894`) replaces it.

## Risks / decisions

- The relay is a **service codefly hosts and operates**; `#648` licensing is not
  in-tree — that is the real weight of Track C, not the client code.
- **Cost:** per-user always-on `e2-standard-8` adds up — Track A's image work must
  include **stop-on-idle** (`#902`).
- Per-user boxes share `platform-portal-dev` Firestore + one WorkOS app (fine —
  they already share dev), but every box's SA needs the cross-project grants.

[remote]: ./remote-daemon-scoping.md
[runbook]: ./byo-remote-runbook.md
[pairing]: ./byor-pairing-scoping.md
