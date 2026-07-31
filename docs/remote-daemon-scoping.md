# Remote daemon, local client — scoping

_Scope of issue [#728][issue-728]: run the session/daemon plane on a server
while the client (TUI or desktop) stays local. This is **Lane 2** of the
monetization spike ([#672][issue-672]), narrowed to "host only the daemon."
This page sizes the work; it is not a pricing or product commitment._

The answer splits cleanly along the same line the
[monetization strategy][monetization] draws between a control plane and an
execution plane:

- **Self-hosted** — "run the daemon on *my* server, connect from my laptop" —
  is architecturally supported today. What remains is documentation and a short
  list of hardening items, not new architecture.
- **Managed / paid** — a per-user cloud daemon we provision, keep warm, and
  bill for — is a product, not a transport change. The transport is free; the
  cost is the hosting control plane, billing, and safe multi-tenant cloud agent
  execution. That work is already sized in the [monetization strategy][monetization]
  (Lane 2) and is **not** re-derived here.

## What works today (self-hosted)

The [client/daemon split][deployment] is real: the daemon owns all state and IO
(PTYs, provider polling, the store, worktrees, git, agents); the TUI is a thin
renderer over IPC. In-process is the default, but the same code path runs
out-of-process over a Unix socket with no branching in the UI.

Two ways to reach a remote daemon exist:

1. **Standalone daemon + `--connect` over SSH** (the recommended path).
   On the server:

   ```sh
   lazybox server start        # long-lived daemon on ~/.lazybox/run/daemon.sock
   ```

   From the laptop, forward the socket and attach a local TUI:

   ```sh
   ssh -L /tmp/lazybox.sock:$HOME/.lazybox/run/daemon.sock user@server
   lazybox --connect /tmp/lazybox.sock
   ```

   The daemon survives client disconnects (same model as a tmux server), and
   terminal replay reconstructs the screen on reconnect from the per-terminal
   ring buffer. SSH is the trust boundary — there is no TCP/TLS in the socket
   transport.

2. **JSON HTTP API gateway** for non-terminal clients (desktop, iOS, browser):

   ```sh
   LAZYBOX_API_TOKEN=secret lazybox server api   # binds 127.0.0.1:8787
   ```

   The gateway is loopback-only and enforces it: it refuses to start without a
   bearer token unless `--insecure-no-auth` is passed, and it **refuses any
   non-loopback bind outright** (`server_api` in `crates/tui-boot/src/main.rs`;
   `ensure_loopback` in `crates/server/src/api_gateway.rs`). Reach it remotely
   through the same SSH tunnel. This is deliberate: bearer auth over plaintext
   HTTP is neither transport encryption nor principal isolation, so a routable
   listener is disabled until TLS and principal-scoped authorization exist.

So the self-hosted "your agents run on your beefy box, your terminal stays on
your laptop" story is real for a **single trusted user on their own box**. The
gap to a *product* is entirely on the managed side.

## Hardening gaps for the self-hosted path

None of these block a single-user self-hosted setup, but they are the sharp
edges to document (and the smallest ones worth closing) before recommending it
broadly:

- **Single-principal credentials.** Provider credentials resolve from the
  daemon **process** environment (`GH_TOKEN` / `gh auth token`, `LINEAR_API_KEY`,
  Slack tokens). A shared daemon therefore acts as one GitHub identity for every
  connected client — there is no per-connection credential scoping. This is
  single-user by design (ROADMAP §6, per-principal credentials). A shared box
  needs one daemon per user (distinct `LAZYBOX_HOME`), not one daemon serving
  several people.
- **Transport security is entirely SSH's job.** The Unix-socket and HTTP
  transports carry no built-in TLS or authentication of their own beyond the
  API bearer token. Any remote use must tunnel; a bare socket or an
  `--insecure-no-auth` gateway on a routable interface would expose full agent
  control (`git`, a shell, the user's tokens) to anyone who can reach it.
- **One socket per `LAZYBOX_HOME`.** A second `server start` against the same
  home contends for the socket; multiple daemons need distinct homes
  (`LAZYBOX_HOME` / `LAZYBOX_RUNTIME_DIR`).
- **Operational lifecycle is manual.** `server start` runs in the foreground;
  a real server wants it under a service unit (systemd / launchd / tmux) with
  restart-on-crash, log rotation, and a boot hook. There is no packaged unit
  file today.

Sizing for the self-hosted path: **mostly documentation plus the small
hardening/ops items above.** No new architecture.

## The managed / paid tier

Turning the self-hosted path into a *paid managed offering* is where the real
work is, and it is the gating risk rather than the transport:

- **Managed hosting** — a per-user cloud daemon (container / microVM),
  provisioned, kept warm, reachable through an authenticated endpoint rather
  than raw SSH, with idle spin-down and acceptable cold-start latency for the
  "check my agents from my laptop" case.
- **Accounts, auth, and billing** — identity, per-user tokens, subscription and
  usage metering, per-run cost attribution.
- **Safe multi-tenant cloud agent execution** — the gating risk. The agent
  drives a real shell, `git`, and the user's tokens, so each user needs genuine
  isolation (a single-tenant sandbox / microVM boundary, not "a container"),
  server-side credential vaulting (GitHub App installation tokens minted just
  in time; BYO model keys behind the existing `llm_gateway_url` proxy rather
  than injected into the shell), default-deny egress, and hard resource limits.
- **Client packaging** — point the local TUI / desktop client at the managed
  endpoint. This is mostly config plus the gateway auth, and reuses the
  [desktop-client boundary][desktop] (#641): the desktop shell already consumes
  the same public daemon contract as the TUI.

The economics (per-user COGS by active sandbox hours, BYO-key model-token
costs), the trust-boundary and execution-controls design, the credential
handling, and the release gates for this tier are all worked out in the
[monetization strategy][monetization] (Lane 2 and "Cloud security and isolation
plan"). This page does not restate them; it records that #728 is the narrowed
"host only the daemon" framing of that lane and that the managed tier stays
gated behind those same security gates.

## Recommendation

1. **Ship the self-hosted runbook now** — the two paths above plus the hardening
   caveats — as the documented answer to "run the daemon on a server, connect
   from my laptop." It is the low-lift, real value today. This page and the
   [deployment feature doc][deployment] are that runbook.
2. **Close the small ops gaps opportunistically** — a sample service unit and
   the multi-daemon-per-user note make the self-hosted path robust without new
   architecture.
3. **Keep the managed tier gated** behind the monetization spike's Lane 2
   sequencing and security release gates. The transport being free does not
   make the product cheap; safe multi-tenant cloud agent execution is the cost,
   and it waits for demonstrated paid demand from Lane 1.

[issue-728]: https://github.com/AntoineToussaint/lazybox/issues/728
[issue-672]: https://github.com/AntoineToussaint/lazybox/issues/672
[monetization]: monetization-strategy.md
[desktop]: desktop-spike.md
[deployment]: features/daemon-and-deployment.md
