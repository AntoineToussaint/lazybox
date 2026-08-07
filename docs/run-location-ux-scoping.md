# Run location UX — "work local vs in a sandbox" + connect from another laptop — scoping (#938)

_The **UX layer** over the remote/sandbox backend. Epic [#885][epic-885] built the
plumbing — a per-worktree remote box ([#888][issue-888]/[#915][issue-915]), the
tunnel supervisor ([#908][issue-908]), sleep/wake lifecycle ([#913][issue-913]),
the relay + QR pairing encoding ([#894][issue-894]), and per-device identity
([#892][issue-892], still open). The backend answers **how a box starts, stops,
and is reached**. This note answers **how a person chooses a run location and
reaches a running one** — the client wiring that turns those primitives into a
visible, first-class choice._

_Sibling notes: [`obin-remote-dev-scoping.md`][obin] (the epic's architecture),
[`byor-pairing-scoping.md`][pairing] (the relay/QR/creds design),
[`desktop-remote-readiness.md`][desktop] (the desktop's remote gaps),
[`remote-daemon-scoping.md`][remote] (the self-hosted baseline). This page sizes
and sequences the UX work and names the decisions; **it is design only — no code
until the sandbox backend trait ([#931][issue-931]) lands** (see §7)._

**Two questions:**

1. **Work local vs work in a sandbox** — make each workspace's run location a
   first-class, create-time choice, persisted on the workspace, and
   **always visible** on inbox rows and terminals.
2. **Connect to an existing sandbox from another laptop** — a "my sandboxes"
   view with power state and a Connect action; attach from this machine (tunnel
   + `--connect`) or another (pairing link/QR); sleep/wake in the UI; surface
   (not rebuild) the reconnect state.

---

## 1. What the client models today — one global switch, no per-workspace choice

The remote-host machinery is **built and persisted at the git-ops / server /
config layers, but wired into exactly one place**: teardown-on-delete. There is
no per-workspace run location and no way for a person to pick one.

- **The choice is a single global config flag, not per-workspace.**
  `remote.host.enabled` (`crates/config/src/lib.rs:139-158`, default `false` at
  `:160-172`) is a master switch: while off every worktree runs on the daemon
  host; while on, *every* worktree stamps a box. It carries one box's shape —
  `project`, `zone`, `machine_type`, `source_image`/`instance_template`,
  `instance_prefix` — with no provider axis and no per-repo or per-workspace
  override. `RemoteHostManager::from_config` returns `None` unless it is enabled
  and complete (`crates/server/src/remote.rs:37-50`), so the whole path is inert
  by default.

- **Nothing on `Workspace` records where it runs.** The struct
  (`crates/core/src/workspace.rs:198-323`) has no run-location / remote / host
  field. Its only locality-adjacent fields are `local: bool` (`:221-222`,
  hand-created vs provider-derived) and `linked_checkout: Option<PathBuf>`
  (`:232-233`, run in an existing on-disk clone instead of an isolated
  worktree) — neither is Local-vs-Sandbox. The full `Workspace` serializes to
  one `workspace:<key>` row (`crates/store/src/traits.rs:238-264`).

- **Create takes no location.** `Command::CreateWorkspace { name, project_key,
  spawn_agent }` (`crates/ipc/src/lib.rs:916-931`) → `create_empty_workspace`
  (`crates/server/src/workspace/mod.rs:20-34`, hardcodes `local = true`) → the
  CLI `workspace create` (`crates/tui-boot/src/main.rs:608-661`: `--name`,
  `--project`, `--repo`, `--agent`, `--cwd`). No run-location flag anywhere.

- **The box handle is already persisted per worktree.** `RemoteHostManager`
  writes the `RemoteHost` handle under `remote-host:<worktree-key>`
  (`crates/server/src/remote.rs:20-22, 92-106`) so reuse survives restart. But
  the only caller in the daemon is best-effort teardown on workspace delete
  (`crates/server/src/workspace/mod.rs:840-846`); the comment there
  (`:836-838`) states **provision-on-open (`ensure`) is deliberately deferred**.

So the ingredients exist (a persisted per-worktree box handle, a provisioner,
power-state queries) but the *product surface* — a per-workspace choice, a
badge, a picker — is entirely net-new.

## 2. Run location as a per-workspace property

The design mirrors the existing `local`/`linked` locality distinction, which
already has the exact shape we want: a persisted per-workspace notion of "where
sessions run," a **sidebar badge** (`⎇ local`), and a **terminal-tab badge**
(`⎇ main`). Run location is the third axis in that family.

**Model.** A new `run_location` field on `Workspace` (`Local | Sandbox {
deployment }`), persisted in the `workspace:<key>` row. Because
`absorb_user_state_from` destructures `Workspace { … }` exhaustively
(`crates/core/src/workspace.rs:755-793`), the field is a compile error until
classified — here it is **identity/structure** (set at create, not merged from a
provider poll), so it destructures to `_`. Adding it bumps
`WORKSPACE_SCHEMA_VERSION` (currently `2`, `crates/core/src/workspace.rs:165`);
the lenient decoder treats an absent field as `Local` for existing rows.

**Create-time choice (v1).** Extend `Command::CreateWorkspace`
(`crates/ipc/src/lib.rs:916-931`), `create_empty_workspace`
(`crates/server/src/workspace/mod.rs:20-34`), and the `workspace create` CLI
(`crates/tui-boot/src/main.rs:608-661`, a `--sandbox[=<deployment>]` flag) to
carry the location. In the TUI the `x n` new-workspace flow
(`ActionKind::NewWorkspace`, `crates/tui-core/src/action.rs:1040-1046`) and the
global `Shift-W` start-agent flow gain a run-location step — a `Choice` modal
(`crates/tui/src/realm/components/choice.rs:87-109`) offering `Local` and each
configured sandbox deployment. The `x` workspace-leader group is where a
per-workspace **run-location** entry belongs, next to `x i` import-checkout
(`:1071-1077`), the sibling locality action.

**Always-visible badge.** Two render sites, both with a direct precedent:

- **Sidebar row** — add a `cell_sandbox` badge to the passive badge cluster
  `cell_badges` (`crates/tui/src/components/workspace_row.rs:862-870`),
  modeled byte-for-byte on `cell_linked` (the `⎇ local` badge, `:883-901`).
  A `run_location`/`is_sandbox` field on `WorkspaceRowCtx` (`:30-123`, next to
  `track_main` at `:106-109`), populated in
  `crates/tui/src/components/sidebar/render.rs:989-1032` alongside `track_main`
  (`:1028`). Suggest `☁ gcp` / `☁ e2b` in `theme.accent` so it reads distinctly
  from the `⎇ local`/`⎇ main` warn-colored branch badges.
- **Terminal tab** — add a locality span to the tab-strip loop
  (`crates/tui/src/components/terminal_stack.rs:3509-3638`), right after the
  `⎇ main` (`:3613-3623`) and `◆ {tier}` (`:3624-3637`) badges, driven by a new
  field on the terminal slot next to `on_main` (`:896`) and `model_label`
  (`:900`), threaded through snapshot sync (`:3074-3075, 3094-3095`) and the
  slot constructor (`:2664-2685`).

The issue's "where am I? must never be ambiguous" is a hard requirement because
**capabilities differ** (§5), not a cosmetic nicety — the badge is the signal a
person reads before assuming a local-only action will work.

**Default policy.** New workspaces default to **`Local`**. A configured default
sandbox is opt-in via config (see below), and — deferred past v1 — a per-repo
override. Local-by-default keeps the zero-config path untouched (matches
`RemoteHostManager::from_config` returning `None` today) and means the heavy,
billable path is never entered by accident.

**Deployment pick.** Today `remote.host` is a *single* box shape. The sandbox
backend ([#931][issue-931]) introduces a **provider/deployment** axis (default
sandbox vs an override like obin). For v1, surface the deployment **in the
create picker** when more than one is configured, and read the roster from
config (a `sandboxes:`/`deployments:` map keyed by name, each a
provider + box shape — the multi-deployment generalization of today's single
`remote.host` block). One configured deployment → no extra prompt; the choice
collapses to `Local | Sandbox`.

**Switching (create-time only for v1).** "Move a workspace local↔sandbox" is a
later nicety — it means draining sessions, moving or re-cloning the worktree,
and re-homing the box handle. **Call it out, do not build it.** v1 is: pick at
create; to change, create a new workspace.

## 3. "My sandboxes" — a cross-machine view

A global list view, distinct from the per-workspace badge: the badge answers
"where does *this* workspace run"; the list answers "what boxes do I have, and
let me reach one from here." Both, per the issue's own conclusion.

**Data.** Read box handles from the store — the `remote-host:*` keys
(`crates/server/src/remote.rs:20-22`) — and enrich each with a live power state
from `GcloudProvisioner::status` → `HostState`
(`crates/git-ops/src/remote.rs:227-231, 65-74`). Rows show: name, provider,
power state (`Running` / `💤 Stopped|Suspended`, from `HostState`), what's on it
(the workspace(s) keyed to that handle), and last-active. `#931` should expose
this as a daemon list command so the client stays store-free per the dep rules.

**Surface.** A new Global list-view action modeled on `OpenSnippets`
(`]`, `crates/tui-core/src/action.rs:888-894`) / `JumpToWorkspace`
(`` ` ``, `:895-901`), added to the Global `ActionKind` group (`:487-513`) with
its `for_kind` arm, `Action` variant, `kind()` arm, and stable string id
(`:1863/:1927`). The view itself follows the read-only **snippet browser**
(`crates/tui/src/realm/components/snippet_browser.rs`,
`mount_snippet_browser` at `keys.rs:702`) or the `FilterableList` jump picker
(`crates/tui/src/realm/components/jump_picker.rs`, mounted at
`crates/tui/src/realm/model/mod.rs:2595-2609`) — a new `Id` variant + a
`PickFlow` arm in `crates/tui/src/realm/model/choice_dispatch.rs` routes the
**Connect** action.

## 4. Connect: attach, wake, reconnect

### Attach from this machine — reuse the landed path

The client attach path exists end to end: the in-process **tunnel supervisor**
(`crates/tui-boot/src/tunnel.rs` — ssh/IAP forward of the daemon socket +
workload ports, capped-backoff respawn) brings up the socket, and
`socket::connect_reconnecting` (`crates/tui-boot/src/main.rs:1014-1058`) dials
it. Connect from the "my sandboxes" view = resolve the box's `TunnelConfig`
(`crates/config/src/lib.rs:287-327`), `bring_up_tunnel`
(`crates/tui-boot/src/main.rs:971-995`), then the existing `--connect`. This is
wiring an existing action to a picker row, not new transport.

### Attach from another machine — the pairing link, gated

The pairing **encoding** landed: `PairingLink { relay_url, box_pubkey, code }`
with `to_url` (`lazybox://pair#…`) / `from_url`
(`crates/ipc/src/pairing.rs:110-149`), a short-TTL `PairingCode`
(`:60-92`), and the workload port-forward mux
(`crates/ipc/src/port_forward.rs`). But it is a **standalone primitive with no
caller** — nothing generates or opens a link, because the two things it needs
are not landed:

- the **E2E channel** that consumes `box_pubkey` is a stub
  (`crates/e2e-channel/src/lib.rs:15-18`), and
- **per-device identity** ([#892][issue-892]) is `local`-only: `PrincipalId`
  exists (`crates/ipc/src/lib.rs:441-490`) but every production construction is
  `PrincipalId::local()`; no per-device credential minting exists.

So "generate link/QR on the box, open on the other laptop → handshake → mint a
revocable per-device cred → attach" is **design-only** here and blocked on
`#891`/`#892`, exactly as [`byor-pairing-scoping.md`][pairing] specifies. The UX
plan: a **Pair a device** action on the account/box (renders the QR from
`PairingLink::to_url`) and an **Open pairing link** entry on the other machine
(`from_url` → relay dial → attach). Both are inert stubs until the E2E channel
and per-device creds land — flag them as such, do not fake the handshake.

### Sleep/wake in the UI

Power state is queryable now (`HostState`, `needs_start()` for
Stopped/Suspended/Terminated, `crates/git-ops/src/remote.rs:89-92`), and the
provisioner already **starts a stopped box before use** inside `ensure`
(`:163-183`) and can `stop` it (`:185-199`). But the *lifecycle policy* shipped
as **box-side shell**, not daemon Rust: `contrib/box-lifecycle/*`
(`connect.sh` = start-on-connect, `lazybox-idle-stop.*` = stop-on-idle; guarded
by `crates/core/tests/box_lifecycle.rs`), and **provision-on-open is deferred**
in the daemon (`crates/server/src/workspace/mod.rs:836-838`).

UX design, riding whatever `#931` exposes:

- **Show power state** in the "my sandboxes" view and on the badge (a `💤` on a
  sleeping box's sandbox badge).
- **Wake-on-connect**: Connect on a sleeping box shows a spinner (the `Loading`
  modal already used by the setup runner, `crates/tui/src/setup_flow.rs`) while
  `ensure`/start runs, then attaches. This needs the deferred provision-on-open
  wired behind `#931`.
- **Manual Sleep** control in the view → the daemon's `stop`.
- **Cost awareness**: an always-on box is billable (per
  [`obin-remote-dev-scoping.md`][obin], `e2-standard-8` is not free) — surface
  last-active and a "still running" hint so an idle box is visible.

### Reconnect — surface, do not rebuild

Already solved and surfaced. The socket layer reconnects, re-`Subscribe`s, and
replays the ring buffer (`crates/ipc/src/socket.rs:440-587`); the client
exposes `ConnectionStatus { Connected, Reconnecting }`
(`crates/ipc/src/lib.rs:3066-3081`); the TUI shows
`"⟳ daemon connection lost — reconnecting…"`
(`crates/tui/src/realm/model/mod.rs:1660`, driven by `tick_connection_status`
at `:3163-3199`); the desktop drains stale input on reconnect
(`apps/desktop/src-tauri/src/main.rs:1043-1077`). **Nothing to rebuild** — the
"my sandboxes" view reuses this state; a reconnecting attached box shows the
existing banner.

## 5. Locality is a capability boundary, not a label

The badge matters because actions already **decline against a remote daemon**,
and this must generalize to sandbox-run workspaces. The canonical precedent:
`editor_unavailable_remote` (`crates/tui/src/realm/model/mod.rs:3684-3697`)
short-circuits the editor with `"editor opens on your machine — unavailable for
a remote daemon; use \`s\` for a server shell"`, keyed off the model's `remote`
flag (`:1107`, set via `with_remote()` at `:1987-1989`); the footer even hides
the editor hint for remote clients
(`crates/tui/src/components/sidebar/mod.rs:2451`,
`sidebar.contextual_bindings(catalog, remote)`). Skill scaffolding declines the
same way (`crates/tui/src/realm/model/inputs.rs:1166, 1186`).

A sandbox-run workspace is a remote-daemon workspace from the client's point of
view, so it inherits this gate. The design principle: **every capability that is
local-only must reflect the workspace's run location**, using the same
flash-and-short-circuit pattern, and the badge is what tells the user *before*
they try. This is the concrete reason the issue calls "where am I?" ambiguity a
defect (`desktop-remote-readiness.md` cited in the issue).

## 6. Open questions — answered

- **One "sandboxes" surface, or a property per row?** **Both**, as the issue
  suspects: a persisted `run_location` **property + badge** per workspace (§2),
  and a **global "my sandboxes" list** for cross-machine connect (§3). They
  answer different questions and share no state beyond the `remote-host:*` keys.
- **TUI or desktop leads for v1?** **TUI.** The desktop cannot reach a daemon it
  did not start and lacks the act-on-work half
  ([`desktop-remote-readiness.md`][desktop]: `#814`/`#815` still open); the TUI
  already has `--connect`, the tunnel supervisor, the reconnect banner, and the
  badge/action machinery. Build the TUI surface first; desktop parity follows
  its own track.
- **obin-internal vs sellable BYOR?** The **run-location choice + badge +
  attach-from-this-machine** (§2, §4-attach) are obin-internal-usable **today**
  on the landed primitives and set the daily-driver bar. The **cross-machine
  pairing** (§4-pair) is the sellable BYOR surface and inherits
  [`byor-pairing-scoping.md`][pairing]'s polish bar — but is blocked on
  `#891`/`#892`, so it ships later.

## 7. What's landed vs net-new, and the gate

| Piece | Status | Anchor |
|---|---|---|
| Per-worktree box handle, persisted | **landed** | `crates/server/src/remote.rs:20-106` |
| Provisioner: start-if-stopped / stop / teardown / status | **landed** | `crates/git-ops/src/remote.rs:65-231` |
| Provision-on-open (`ensure` from session-open) | **deferred** | `crates/server/src/workspace/mod.rs:836-838` |
| `--connect` + tunnel supervisor + reconnect/resync | **landed** | `crates/tui-boot/src/{main.rs,tunnel.rs}`, `crates/ipc/src/socket.rs:440-587` |
| Reconnect banner (TUI) + desktop drain | **landed** | `mod.rs:1660,3163-3199`; `apps/desktop/src-tauri/src/main.rs:1043-1077` |
| Stop-on-idle / start-on-connect policy | **landed as box-side shell** | `contrib/box-lifecycle/*` |
| Pairing link/QR + port-forward mux | **landed as unwired primitive** | `crates/ipc/src/{pairing.rs,port_forward.rs}` |
| E2E channel; per-device `PrincipalId` | **stub / `local`-only** | `crates/e2e-channel/src/lib.rs:15-18`; `crates/ipc/src/lib.rs:441-490` |
| Sandbox backend trait (generic start/stop/connect, provider axis, deployment roster) | **open** | [#931][issue-931] |
| Per-workspace `run_location`; badge; "my sandboxes" view; wake-on-connect UI | **net-new (this issue)** | — |

**The gate.** The per-workspace run-location field, the "my sandboxes" list, and
wake-on-connect all call a **daemon-side sandbox API that does not exist yet** —
provision-on-open is deferred and `#931` will define the provider-agnostic
start/stop/connect trait and the deployment roster this UX picks from. Writing
the client now would either hardcode against the single-box `remote.host` shape
(a dead end once `#931` generalizes it) or invent the trait in the UI layer
(wrong crate, per the dep rules). **So this note ships as design; the code lands
against `#931`.**

## 8. Sequencing — the client-wiring follow-ups (post-#931)

Ranked, each a tight PR once the backend trait lands:

1. **`run_location` on `Workspace`** — the field, schema bump, create-command
   plumbing, default `Local`. The foundation; everything else reads it.
   (`workspace.rs`, `ipc/src/lib.rs`, `server/src/workspace/mod.rs`,
   `tui-boot/src/main.rs`.)
2. **Locality badges** — `cell_sandbox` in the sidebar row + the terminal-tab
   span, modeled on `⎇ local` / `⎇ main`. Pure render; unblocks "where am I?".
3. **Create-time picker** — the run-location step in `x n` / `Shift-W`, reading
   the deployment roster.
4. **"My sandboxes" view + attach-from-this-machine** — the list over
   `remote-host:*` with `HostState`, wired to the existing tunnel + `--connect`.
5. **Sleep/wake in the view** — power-state display, wake-on-connect spinner
   (needs provision-on-open wired), manual Sleep, cost hint.
6. **Capability gates** — extend the `editor_unavailable_remote` pattern to
   every local-only action for sandbox-run workspaces.
7. **Cross-machine pairing** — Pair-a-device (QR) + Open-pairing-link, **blocked
   on `#891` (E2E) and `#892` (per-device creds)**; the sellable BYOR surface,
   shipped last.

## Verdict

The run-location choice is a **first-class product concept the client does not
yet express** — the plumbing is persisted per worktree but never surfaced, and
one global config flag stands in for a per-workspace decision. The design is
low-risk because every piece has a landed precedent: `run_location` mirrors
`local`/`linked`, its badges mirror `⎇ local`/`⎇ main`, its capability gate
mirrors `editor_unavailable_remote`, and attach + reconnect are already wired.
The genuinely-blocked half is **cross-machine pairing** (`#891`/`#892`) and the
**deployment/provider abstraction** (`#931`). Ship this as the design; land the
seven follow-ups against the backend trait.

[epic-885]: https://github.com/AntoineToussaint/lazybox/issues/885
[issue-888]: https://github.com/AntoineToussaint/lazybox/issues/888
[issue-892]: https://github.com/AntoineToussaint/lazybox/issues/892
[issue-894]: https://github.com/AntoineToussaint/lazybox/issues/894
[issue-908]: https://github.com/AntoineToussaint/lazybox/issues/908
[issue-913]: https://github.com/AntoineToussaint/lazybox/issues/913
[issue-915]: https://github.com/AntoineToussaint/lazybox/issues/915
[issue-931]: https://github.com/AntoineToussaint/lazybox/issues/931
[obin]: ./obin-remote-dev-scoping.md
[pairing]: ./byor-pairing-scoping.md
[desktop]: ./desktop-remote-readiness.md
[remote]: ./remote-daemon-scoping.md
