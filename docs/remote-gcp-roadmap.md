# Remote-work-on-GCP — what's next (post-merge review)

_Written for [#1014][issue-1014], picked up after the remote/desktop/sandbox
merge backlog cleared (#976/#975/#972/#977 and the sandbox+relay tracks landed
on main). This is the re-scope the placeholder issue asked for: a clear-headed
review of the remote-work-on-GCP path and a prioritized plan for what's next.
Issue states below reflect the repo as of 2026-08; treat them as a snapshot,
not a live tracker._

_Reads on top of the sibling scoping notes — [`remote-daemon-scoping.md`][remote]
(#728), [`byo-remote-runbook.md`][runbook] (#742), [`obin-remote-dev-scoping.md`][obin]
(epic #885), [`desktop-remote-readiness.md`][readiness] (#806) and its live
companion [`desktop-dogfood-log.md`][dogfood] (#837). Where those size the work,
this one records what actually landed and picks the next moves._

## TL;DR

The remote-server + local-client foundation is **built and on main**, across
all four layers the scoping docs laid out:

- **Transport** — TUI `--connect` over `ssh -L` (#742), desktop connect-to-remote
  gateway + reconnect/resync (#814/#896), protocol version-skew tolerance
  (#815). All landed.
- **The productized link (Track C)** — Noise/X25519 E2E channel (#891),
  per-device identity beyond `local` (#892), rendezvous relay + `lazybox serve`
  (#893), QR/link pairing with workload-port forwarding (#894), and the
  entitlement-gate stub (#895). All closed; the `relay` crate is in-tree.
- **The box glue (Track B)** — systemd units (#887), remote-host targeting
  (#888), the tunnel supervisor (#889), and the `dev up` post-create hook (#890).
  Plus the generic start/stop/connect **sandbox engine** (#937, GCP first), the
  r-spawn wired to it (#965), and provisioning that installs a **build-matched**
  daemon so the #815 skew case can't arise for boxes we stamp (#977).
- **Desktop parity** — Tier A/B/C act-on-work + automation + terminals
  (#816/#817/#818), the deferred slice (#843), then the daily-driver hardening
  pass (#972/#975/#976). Parity is essentially closed; stability polish
  (#970/#974, both open) and an empirical sign-off ([`desktop-dogfood-log.md`][dogfood]
  leaves its verdict unsigned) are what remain before it's the primary **local** UI.

What is **not** yet closed is the last mile of each layer: the obin-box demo
itself (#886), an **empirical** remote-box dogfood (nobody has lived on a GCP
box for a week yet), and the security posture that keeps the routable/relay path
SSH-grade rather than a soft target. That last-mile is what "what's next" below
prioritizes.

## Where we actually are — the dogfood truth

**What works on a GCP box today.** You can `lazybox sandbox ensure` a GCE box,
have it stamp a build-matched daemon under systemd, `connect` over IAP/SSH (or
the relay), drive agents from the TUI **or** the desktop, and forward the
workload's `:3000`/`:8082` so the browser stays `localhost`-clean for WorkOS.
The r-spawn (`r c`/`r x`/`r u`) fans agent spawns onto the box from the sidebar.
Reconnect/resync survives sleep/wifi/tunnel-reset because the framed wire and
ring-buffer replay were solved for the socket path and inherited unchanged.

**What's thin.** The dogfood log's **Phase 3** (live-on-a-remote-box) is still
open — the readiness verdict was proven in code, not in a week of real use. The
sharp edges the scoping docs flagged as "document, don't block" are still the
sharp edges: one daemon = one GitHub identity, transport security is entirely
SSH's/the-relay's job (no TLS in the listeners themselves), and lifecycle/cost
control leans on `contrib/box-lifecycle/` + #902/#978 rather than being a
first-class product surface.

## Cost posture

The obin GCE box is an `e2-standard-8` at **~$210/mo running 24/7**. Stop-on-idle
landed (#902, hardened by #978 so a box whose agent is waiting on a child process
isn't reaped) and ships as `contrib/box-lifecycle/` (idle-stop timer +
`connect.sh` wake-on-connect). **Action:** confirm the timer is actually armed on
the live obin box and right-size the machine type — an `e2-standard-8` is
demo-generous; measure the real agent+stack footprint and drop a tier if it fits,
which roughly halves idle burn. This is cheap and should happen before any wider
dogfood invitation.

## Gaps ledger (from the scoping docs, re-checked)

| Gap | Status | Next move |
|---|---|---|
| TLS / routable listener (both transports are loopback-only by design) | Still SSH/relay-only. The relay (#893) carries ciphertext over its own channel, but a *bare* routable gateway/socket has no TLS of its own. | Keep loopback-only; make the **relay** the one blessed off-box path. Only revisit a native TLS listener if a non-relay routable need appears. |
| Single-principal daemon credentials | Unchanged — provider creds resolve from daemon process env; one daemon acts as one GitHub identity. Per-device identity (#892) authenticates the *link*, not the *provider scope*. | Document loudly (one daemon per user, distinct `LAZYBOX_HOME`); real per-principal cred scoping stays gated behind the managed tier. |
| Service-unit / ops | Shipped — `contrib/systemd/` (#887) + `contrib/box-lifecycle/`. | Fold into the sandbox provisioner so a stamped box arrives with them armed, not hand-installed. |
| Session-bearing-workspace safety (post-#924) | Closed — account-switch no longer prunes live worktrees/sessions. | Add the remote-box variant to the Phase-3 dogfood checklist (account switch while agents run **on the box**). |
| Empirical remote dogfood | Open (#837 Phase 3). | Prioritized as **P0** below. |

## What's next — prioritized

**P0 — Prove the loop, not just the plumbing.** Close the obin-box demo (#886)
and run the dogfood-log **Phase 3** for real: one person lives on a GCP box for a
week, driving agents + the obin web app from the desktop over the relay, logging
every fallback. The foundation is code-complete; the open risk is that nobody has
*lived* on it. Everything below is lower-value until this produces a punch list.
Cheap prerequisite: verify stop-on-idle is armed and right-size the box (see Cost).

**P1 — Harden the last mile the Phase-3 run will expose.** Expect the fallbacks
to cluster in: (a) reconnect/resync under real network churn on the *relay* path
(the socket path is tested; the relay hop is newer), (b) lifecycle friction —
wake-on-connect latency, idle-stop false positives/negatives, and the box
arriving without its systemd/idle units armed, and (c) the single-principal
rough edges when more than one person eyes the same box. Fix these as the
dogfood surfaces them rather than pre-emptively.

**P2 — Decide the monetization lane deliberately, don't drift into it.** The
managed-tier stubs exist (entitlement gate #895) and two hosted sandbox backends
are queued (#942 E2B, #943 k8s). Per [`remote-daemon-scoping.md`][remote] the
transport is free and the cost is safe multi-tenant execution + billing —
explicitly gated behind demonstrated paid demand. **Recommendation:** hold #942/#943
until the desktop v1 (#648) and its release mechanics (#1010 Apple signing,
#1011 Setapp spike) establish there's a paying audience. Building a second/third
sandbox backend before the first product ships is premature breadth.

**P3 — Documentation convergence.** Five overlapping scoping docs now describe a
built system in the future tense. Once Phase 3 closes, collapse the "will it
work?" framing into one runbook + this roadmap, and mark the readiness/scoping
notes historical (the readiness doc already did this for its own verdict). Not
urgent, but it's the difference between a legible remote story and archaeology.

## Explicitly not now

- **Native TLS in the listeners.** The relay is the blessed off-box path; a
  second encrypted-transport story is redundant surface until a non-relay
  routable need is real.
- **Per-principal provider credentials / multi-tenant daemon.** Gated behind the
  managed tier and its security release gates (monetization Lane 2). One daemon
  per user stays the answer for self-hosted.
- **More sandbox backends (#942/#943).** See P2 — sequenced after desktop v1
  proves demand.

[issue-1014]: https://github.com/AntoineToussaint/lazybox/issues/1014
[remote]: remote-daemon-scoping.md
[runbook]: byo-remote-runbook.md
[obin]: obin-remote-dev-scoping.md
[readiness]: desktop-remote-readiness.md
[dogfood]: desktop-dogfood-log.md
