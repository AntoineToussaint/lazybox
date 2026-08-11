# Setapp distribution spike

_Research checked 2026-08-11. Setapp requirements and revenue mechanics are
from MacPaw's public developer docs (linked below); confirm the entitlement,
usage-reporting, and price-tier details with the Setapp review team before any
build. This is a feasibility report, not a distribution commitment._

## Question

Can — and should — the lazybox **desktop app** ship on
[Setapp](https://setapp.com/developers), MacPaw's Mac-app subscription
marketplace, as a monetization path? The crux is technical: lazybox desktop
runs a local daemon, owns PTYs, spawns agent CLIs (`claude` / `codex` / `gh`),
and manages git worktrees across the filesystem. Setapp's catalog is full of
self-contained apps; ours shells out heavily. Feasibility hinges on whether
Setapp's distribution model permits that.

## Answer

**Technically feasible — the crux passes.** Setapp does **not** require App
Sandbox and does **not** mandate the hardened runtime. It accepts the same
artifact `release-desktop.yml` already produces: a **Developer ID–signed,
notarized, universal** `.app`. Because there is no App Sandbox, there is no
entitlement wall around `posix_spawn`/`fork`+`exec`, arbitrary filesystem
access, or git — the exact capabilities lazybox depends on keep working. This
is the decisive difference from the **Mac App Store**, whose mandatory sandbox
would block process-spawning and free FS access outright and effectively rule
lazybox out without a major re-architecture.

**Recommendation: no-go as a primary channel; revisit as a secondary discovery
channel later.** The blocker is not engineering, it is business model and
policy fit. Setapp pays from a **usage-weighted revenue pool**, forbids any
in-app licensing/activation or paid features, and takes over the update
channel. That collides with the Lane 1 plan in
[monetization-strategy.md](monetization-strategy.md) — sell a polished desktop
client directly, keep the customer relationship, and use it as the on-ramp to
Lane 2 hosted compute. Setapp trades all of that for an unpredictable,
diluted, catalog-shared payout. Ship the direct **Homebrew cask + own
subscription** first; treat Setapp as an experiment only after that has paying
users and only if it can run *alongside* the direct channel.

## Feasibility detail: the sandbox / process-spawning crux

| Requirement | Setapp | Mac App Store | lazybox today (`release-desktop.yml`) |
|---|---|---|---|
| Developer ID signature | Required | N/A (uses App Store cert) | ✅ Yes |
| Notarized + stapled | Required | N/A (App Store review) | ✅ Yes |
| Universal (`arm64` + `x86_64`) | Required | Required | ✅ Yes |
| **App Sandbox** | **Not required** | **Required** | ❌ Not sandboxed |
| Hardened runtime | Not mandated | Required | Tauri default |
| Spawn external processes (`claude`/`gh`/`git`) | **No sandbox obstacle** (confirm w/ review team) | Blocked without unavailable temp exceptions | Core dependency |
| Arbitrary FS / git worktrees | **No sandbox obstacle** (confirm w/ review team) | Blocked (user-selected scope only) | Core dependency |

The [Setapp app-preparation requirements](https://docs.setapp.com/docs/preparing-your-application-for-setapp)
list notarization, Developer ID signing, and a universal binary as the
technical bar and are silent on App Sandbox — Setapp distributes notarized
direct-download apps, not App Store sandboxed ones, so the catalog already
includes utilities that touch the filesystem and shell out. The
already-signed-and-notarized universal `.app` is exactly the accepted artifact,
so **the packaging baseline is done**. Get the process-spawn/FS posture
confirmed in writing by the review team (developers@setapp.com) before
committing, but there is no known sandbox obstacle.

## Required changes if we proceed

The baseline artifact is reusable; the net-new work is an entitlement client, a
policy-compliant build variant, and the review cycle.

1. **Setapp framework integration (the real engineering).** Embed the
   [Setapp framework](https://github.com/MacPaw/Setapp-framework) to (a) verify
   the user has an active Setapp subscription and (b) report usage after the
   user exercises the app's main functionality (Setapp computes revenue from
   these reports). The framework is **Swift/Objective-C first**, with wrappers
   for Electron, Flutter, and Node — but, per the framework README's listed
   integrations, **no first-class Tauri/Rust binding** (not verified beyond the
   README — confirming this is spike step 1, since a community binding would
   shrink the estimate). For our Rust-shell Tauri app that most likely means
   either linking the macOS `xcframework` from Rust over an Objective-C FFI
   shim, or bundling a tiny native helper the shell talks to. This is the bulk
   of the effort and the main unknown. The check should run at launch / new
   session, matching Setapp's guidance to re-verify each time the user accesses
   the app.

2. **A separate, policy-compliant build target.** Setapp **forbids proprietary
   installers, self-update frameworks, and activation/licensing mechanisms**,
   and forbids in-app stores / paid features. So a Setapp build must:
   - **strip the self-update path**: the build-guard update modal
     (`crates/tui/src/build_guard.rs`) and the Homebrew-cask/cargo-dist
     upgrade prompts must be compiled out or made inert — Setapp owns updates.
     (The desktop already relies on the cask as its updater, not an in-app
     Tauri updater, so there is no Tauri auto-updater to remove — just the
     "newer version, run `brew upgrade`" surfacing.)
   - **carry no lazybox-side license gate**: every feature must be included for
     the Setapp subscriber; the Setapp entitlement is the *only* gate. BYO-daemon
     and remote/BYOR modes stay (they are configuration, not paid tiers), but we
     cannot layer our own subscription or paid add-on inside the Setapp build.
   - probably use a **distinct bundle identifier / product config** so the
     Setapp build and the Developer-ID cask build don't collide on one machine.

   This fits the existing "separately distributed product" boundary already
   described in [desktop-spike.md](desktop-spike.md#packaging-and-licensing-boundary):
   the Setapp variant is another downstream packaging of the shared engine, with
   its own entitlement client and updater policy, not a fork of daemon boot.

3. **A Setapp release lane.** A sibling of `release-desktop.yml` that builds the
   Setapp variant (framework linked, self-update stripped), signs + notarizes it,
   and uploads to the Setapp vendor portal instead of the Homebrew tap. Same
   universal-`.app` machinery; different embedded framework and upload target.

4. **Vendor onboarding + review.** Setapp vendor account, app listing, price
   tier assignment, and a review pass (they test main functionality on the
   latest macOS). Expect back-and-forth, especially given the unusual
   shell-out/agent-spawn behavior — worth flagging to reviewers proactively.

### Licensing/gating interaction with BYO-daemon / remote modes

Gating is clean because the Setapp entitlement replaces — rather than composes
with — any lazybox license. The desktop already keeps the daemon, gateway
token, and credentials in the Rust shell; the Setapp check is one more
launch-time gate in that shell before it binds the gateway. BYO-daemon and
remote (`sandbox:` / BYOR) connections are just where the client points and
carry no payment logic, so they are unaffected as long as we add **no** paid
tier of our own inside the Setapp build (which the policy forbids anyway).

## Revenue model fit

Setapp's [revenue distribution](https://docs.setapp.com/docs/setapp-membership-revenue):
consumers pay a flat monthly subscription (typically **$9.99–$19.99**); **70%**
of each user's fee is split among the developers of the apps *that user actually
opened* during the period, weighted by a **price-tier multiplier** (tiers 1–17,
`1x`→`100x`, assigned from the app's standalone/annual list price); a separate
**+20% partner fee** rewards users you personally referred to Setapp; usage
during the 7-day trial generates no revenue.

Fit for a dev tool, honestly assessed:

- **Engagement cuts both ways.** lazybox is a leave-it-open daily driver, so
  per-subscriber usage share is favorable *when* a subscriber uses it. But the
  70% pool is split across *every* app that subscriber opened, and payout is
  proportional to usage × tier, not to how essential the app is.
- **Diluted and unpredictable.** Earnings depend on the whole catalog's usage
  mix and each subscriber's app portfolio — the docs themselves frame it as
  only *moderately* predictable. There is no fixed per-install or per-seat
  revenue to model, unlike a direct $12/mo or $99/yr subscription.
- **Tier ceiling.** To land a high multiplier we must maintain a high public
  standalone price (with ≥3 months of documented price history before
  re-tiering). A dev-tool audience willing to pay a high direct price is
  exactly the audience we'd rather bill directly at full margin.
- **No customer relationship.** Setapp owns billing, the user list, churn
  signals, and the upgrade surface. That severs the Lane 1 → Lane 2 on-ramp
  (desktop buyers becoming hosted-compute users) that
  [monetization-strategy.md](monetization-strategy.md) treats as the strategic
  point of selling the desktop at all.

## Effort estimate

**T-shirt: M (medium), ~2–4 focused weeks + review latency.** No fundamental
re-architecture — the sandbox crux passes and the signed universal build
exists. Effort breakdown:

| Piece | Size | Note |
|---|---|---|
| Setapp framework binding from Rust/Tauri | **M** | The real work; no first-class Tauri binding — FFI shim or native helper |
| Entitlement check + usage reporting wiring | S | Launch/session gate in the existing Rust shell |
| Policy-compliant build variant (strip self-update, no license gate, distinct id) | S | Config + conditional compile of `build_guard` surfacing |
| Setapp release lane (CI) | S | Fork of `release-desktop.yml`, different upload target |
| Vendor onboarding + review cycle | **M (calendar, not code)** | External latency, plus review scrutiny of the shell-out behavior |

The dominant risks are the **Rust↔Setapp-framework binding** (unproven path)
and **review latency/scrutiny** for an app that spawns arbitrary CLIs — not the
packaging, which is solved.

## Go / no-go vs. alternatives

| Option | Monetization | Customer relationship | Effort | Lane-2 on-ramp | Verdict |
|---|---|---|---|---|---|
| **Direct cask + own subscription** (Lane 1) | Full price, full margin | Owned | Scoped in #641 | Intact | **Primary — do this first** |
| **Setapp** | Usage-pool share, unpredictable, diluted | Owned by Setapp | +M (framework + review) | Severed | Secondary experiment, later |
| **Mac App Store** | Apple commission (15% under the Small Business Program, else 30%), direct | Apple-mediated | **XL / likely infeasible** | n/a | No — sandbox blocks process-spawning |

**Recommendation:** proceed with the direct Homebrew-cask + own-license path
already planned (Lane 1). **Defer Setapp** to a later, optional experiment once
the direct channel has paying users — and only if it can run as an *additional*
discovery channel next to direct sales, not a replacement. Setapp's real
appeal (no billing/licensing infra to build) is genuine but is outweighed by
unpredictable pool-share revenue, loss of the customer relationship, and the
policy ban on our own licensing that would otherwise carry the Lane 1 → Lane 2
strategy. The Mac App Store is a firm no: its mandatory sandbox blocks the
process-spawning and filesystem access lazybox is built on.

## Sources

- [Setapp — preparing your application](https://docs.setapp.com/docs/preparing-your-application-for-setapp)
- [Setapp — submitting apps for review](https://docs.setapp.com/docs/submitting-apps-for-review)
- [Setapp — membership revenue](https://docs.setapp.com/docs/setapp-membership-revenue)
- [Setapp framework (MacPaw/Setapp-framework)](https://github.com/MacPaw/Setapp-framework)
- [Setapp for developers](https://setapp.com/developers)
- Related: [monetization-strategy.md](monetization-strategy.md),
  [desktop-spike.md](desktop-spike.md), `.github/workflows/release-desktop.yml`;
  issues #648 (paid desktop v1), #672 (monetization strategy), #990 (desktop
  release), #641 (desktop reuse scope).
