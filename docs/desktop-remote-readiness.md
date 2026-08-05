# Desktop app readiness — can it be the daily driver against a remote daemon?

_Readiness assessment for [#806][issue-806]. Follow-up to desktop v1 ([#648][issue-648])
and the BYO-remote work ([#742][issue-742]). Sibling scoping notes:
[remote-daemon-scoping.md][remote] (#728), [desktop-spike.md][spike] (the boundary),
[byo-remote-runbook.md][runbook] (the TUI's SSH path). The running dogfood log
that this assessment kicks off lives in [desktop-dogfood-log.md][dogfood-log]
(#837)._

**Question:** run the daemon on a remote box (PTYs, worktrees, provider polling,
long-running agents) and use the **desktop app** as the local UI over that link —
so we largely stop using the TUI. Is that wired end to end today?

**Go / no-go: NO-GO today**, on two independent axes:

1. **Remote transport.** The desktop cannot connect to a daemon it did not start.
   It always spawns an in-process daemon and binds a loopback gateway; there is no
   "point at a remote box" mode at all. (#742's remote path is TUI-only.)
2. **Feature parity.** Even against its own *local* embedded daemon, the desktop
   covers only the triage half of the workflow. The act-on-work half — diff,
   merge, archive, automation policies, agent/model choice — has no desktop path,
   so you fall back to the TUI regardless of where the daemon runs.

The terminal experience — the whole reason to switch — is the bright spot: on
rendering it is already **better** than the TUI. The blockers are connectivity
and breadth, not the terminal.

---

## 1. Remote-daemon connection — does it work? No.

The desktop shell's `start_desktop_state` unconditionally starts an in-process
`ClientRuntime` from the local `ServerConfig`, binds an **ephemeral IPv4 loopback**
TCP gateway (`127.0.0.1:0`), mints a per-launch random bearer, and points the
webview client at `http://<that-local-addr>`
(`apps/desktop/src-tauri/src/main.rs:1381-1420`). Client and daemon are therefore
the same process and the same build.

- **No knob to reach a remote daemon.** `GatewayClient.base_url` is only ever the
  self-bound loopback address (`main.rs:276-277, 1416-1420`). The desktop reads no
  `LAZYBOX_*` URL/host env var or CLI arg for the gateway. There is no
  "connect-to-existing-gateway, skip-spawn" mode.
- **#742 doesn't help the desktop.** BYO-remote wired the **TUI**: a long-lived
  `lazybox server start` on a Unix socket, reached with `lazybox --connect` over
  `ssh -L` (`crates/tui-boot/src/main.rs:355-366`; `docs/byo-remote-runbook.md`).
  That transport is **length-prefixed bincode over a Unix socket**
  (`crates/ipc/src/socket.rs`, `crates/ipc/src/transport.rs`) — a different wire
  from the desktop's HTTP/JSON gateway. The desktop cannot reuse it.
- **The gateway is loopback-only by design.** `serve_listener` refuses any
  non-loopback bind via `ensure_loopback` (`crates/server/src/api_gateway.rs`),
  and there is no TLS — plain HTTP/1. Remote use is explicitly meant to be an
  **SSH-forwarded loopback port**; a routable listener is deferred until TLS +
  principal-scoped authorization exist (`docs/desktop-spike.md:79-89`,
  `docs/remote-daemon-scoping.md`). So the intended remote shape is: run
  `lazybox server api` on the box, `ssh -L` its loopback port to the laptop, and
  have the desktop connect to `http://127.0.0.1:<forwarded-port>`. **The one
  missing piece is the last clause** — the desktop has no way to be told that.
- **Reconnect.** The desktop's HTTP streams already re-dial on drop with a 750 ms
  backoff (`main.rs:903-916, 986-999`), but they lack the #742 socket path's
  resync guarantee (re-`Subscribe` + authoritative ring-buffer replay,
  `crates/ipc/src/socket.rs:508-690`). A real remote link (sleep, wifi change,
  tunnel reset) needs that.

→ Tracked as **[#814][issue-814]** (connect-to-remote + reconnect/resync).

## 2. Protocol / wire compatibility — the version-skew hard-fail

The desktop's protocol check is a **strict exact-match**. `validate_protocol`
rejects the daemon on any of `protocol_version`, `protocol_fingerprint`, or
terminal-transport mismatch (`apps/desktop/src-tauri/src/main.rs:1470-1490`); the
server enforces the same per request via the `x-lazybox-protocol-fingerprint`
header, returning HTTP 426 (`crates/server/src/api_gateway.rs:990-1005`).

The fingerprint is an FNV-1a hash over `lazybox_ipc::PROTOCOL_FINGERPRINT` **and
the entire source of `api_gateway.rs`** (`crates/server/src/api_gateway.rs:86-103`),
and `PROTOCOL_FINGERPRINT` itself hashes `ipc/src`, `core/src`, **and `Cargo.lock`**
(`crates/ipc/build.rs:58-82`). Any build difference — a dependency bump, even a
comment edit in a wire crate — flips it.

Today this is invisible because the desktop embeds its own daemon (same build).
But the #806 topology means the box and the laptop are two independently-built
binaries: **any skew hard-fails at startup** (`main.rs:1421-1429`) or 426s per
request. `GET /v1/protocol` is discovery, not negotiation — there is no compatible
subset or downgrade. This is a real operational blocker: "the box runs one release,
the laptop another" bricks the link.

→ Tracked as **[#815][issue-815]**.

## 3. Terminal experience — the bright spot

The desktop renders terminals with **xterm.js** (`@xterm/xterm@6`, `@xterm/addon-fit`;
`apps/desktop/package.json:12-14`, instance at `src/main.ts:1246`). On the pain
point that motivates #806 it is already **better** than the TUI:

- **Native scrollback** of 10k lines (`main.ts:1252`), OS-level **text selection**,
  and mouse-wheel scroll — all free from the DOM renderer, versus the TUI's
  manual `Shift-PgUp/PgDn` over libghostty-vt (the standing scrolling pain).
- **Auto-fit resize** via `FitAddon` + a debounced resize handler that resizes the
  server PTY to match (`main.ts:1275-1299, 1479-1486`).
- **Reconnect/replay is wired**: a `Reset` stream item clears the frame decoder on
  reconnect (`main.ts:665-688`, `terminal.ts:149-163`), and `handleTerminalReplay`
  resets xterm and writes the daemon's ring-buffer replay, with sequence-gap
  detection triggering a resync (`main.ts:1362-1414`). The ring is server-side
  (`crates/server/src/api_gateway.rs:48-49`).

**Closed in [#818][issue-818]:** the desktop now mounts every terminal of the
selected workspace concurrently — a tile per runner with a tab strip — so an agent
and a shell (or several agents) stay live side by side without teardown, keyed by
terminal id in `liveTerminals` (`main.ts`). Focus mode (`.`, or ⌘/Ctrl-`.` from
inside a terminal) expands the focused tile across the workspace, hiding the inbox
and activity panels, mirroring the TUI's `TerminalStack` + focus mode.

## 4. Feature parity — the act-on-work half is missing

The desktop's entire mutating vocabulary is **8 `DesktopCommand` variants**
(`crates/server/src/api_gateway.rs:388-425`): `SpawnAgent`, `SpawnShell`,
`CreateWorkspace`, `FocusWorkspace`, `MarkRead`, `PostReply`, `DeliverSnippet`,
`Refresh` — plus local `set_sort_mode` / `set_filters` / `set_search`. The TUI
action catalog has ~80 actions (`crates/tui-core/src/action.rs:402-487`).

**Covered (the triage + drive-an-agent loop):** grouped inbox with shared grouping/
sort, navigate + open, read/unread + reply (GitHub-only), spawn agent/shell, live
terminal, snippets picker, filter/search, new workspace, refresh.

**Tier A — landed (#816):** agent + model-tier + on-main choice per spawn
(`SpawnAgent` now carries `model_alias` / `on_main`, plus `SpawnShell { on_main }`);
merge PR / update branch (`MergePr` / `UpdateBranch`); archive / close issue /
delete-or-close (`Archive` → `Kill`, `CloseIssue`, `DeleteOrClose`); open in
browser (`open_url`, shared launcher); cycle mailbox (`set_mailbox` over the
shared `Mailbox`); rename workspace (`RenameWorkspace`). Exposed through the
workspace actions menu + a mailbox control.

**Tier A — still missing:** view diff (`action.rs:412`) — needs the
`InspectWorkspaceDiff` → `WorkspaceDiffInspected` request/response plus a diff
reader, tracked separately; open in editor (`:411`) — needs terminal-spawn-with-
command semantics the one-shot command path doesn't model.

**Missing — Tier B/C (hands-off + batch + navigation):** automation policies
(merge-on-green / auto-fix / track-main, `:425-428`); reviewers/assignees/labels
(`:433-435`); snooze (`:419-420`); multi-select + broadcast (`:448-449`); repo pin
(`:447`); session adopt/send/convert/collapse (`:429-432`); quick-jump navigation
and focus mode (`:476-479`); theme picker (`:474`); activity-pane row interactions
(`:452-459`).

→ Tracked as **[#816][issue-816]** (Tier A) and **[#817][issue-817]** (Tier B/C).

## 5. Setup friction

Today there is no remote setup to describe, because the connection doesn't exist —
the desktop only runs its own local daemon (`npm run tauri dev`, or the
`lazybox-macos-dogfood.tar.gz` bundle; `apps/desktop/README.md`). Once #814 lands,
the intended manual path is: run `LAZYBOX_API_TOKEN=… lazybox server api` on the box,
`ssh -L <localport>:127.0.0.1:<gatewayport> user@box`, and start the desktop pointed
at the forwarded port with that token. Sharp edges: one daemon per user
(single-principal credentials, `docs/remote-daemon-scoping.md`), manual lifecycle
(no service unit), and the §2 version-skew requirement that both ends be the same
build.

## 6. Packaging

The desktop MVP CI job (`desktop MVP (macos-14)`, `.github/workflows/ci.yml`)
regenerates the contract and fails on any drift in `apps/desktop/src/generated`
(`compatibility.json`, `terminal-wire.ts`) via `git diff --exit-code` — a solid
gate keeping client and server fingerprints locked. But the output is a **macOS-only,
debug, unsigned/un-notarized** dogfood tarball; signing, notarization, and the
updater are explicitly out of scope for now (`docs/desktop-spike.md:99-102, 120-122`).

---

## Ranked dogfood blockers

The "I hit this and went back to the TUI" list, most-blocking first:

1. **Can't connect to a remote daemon at all** — no remote/existing-gateway mode
   (`main.rs:1381-1420`). **[#814][issue-814]**
2. **Act-on-work parity gap** — no diff / merge / archive / browser / agent+tier
   choice / mailbox. You cannot finish a PR from the desktop. **[#816][issue-816]**
3. **Version-skew hard-fail** — box and laptop must be byte-identical builds or the
   link bricks at startup. **[#815][issue-815]**
4. **Automation policies missing** — no merge-on-green / auto-fix / auto-merge, the
   hands-off core of lazybox. **[#817][issue-817]**
5. ~~**Single terminal only** — no tiles/tabs/focus; switching workspace tears the
   terminal down.~~ Closed in **[#818][issue-818]**: concurrent tiles + tabs + focus mode.
6. **Reconnect robustness** over the remote link (resync parity with the socket
   path). Folded into **[#814][issue-814]**.

## Verdict

**No-go** on dogfooding the desktop as the primary UI against a remote daemon
today, and **no-go** even as a primary *local* UI until the Tier-A parity gaps
close. The architecture is sound and the terminal is genuinely the upgrade #806
wants — the work is (1) a remote-connect mode + reconnect, (2) closing the
act-on-work parity gap, and (3) a version-skew story. When #814 + #816 land, the
desktop is a credible daily driver for the local case; #815 + #817 + #818 make the
remote-box vision real.

[issue-806]: https://github.com/AntoineToussaint/lazybox/issues/806
[issue-648]: https://github.com/AntoineToussaint/lazybox/issues/648
[issue-742]: https://github.com/AntoineToussaint/lazybox/issues/742
[issue-814]: https://github.com/AntoineToussaint/lazybox/issues/814
[issue-815]: https://github.com/AntoineToussaint/lazybox/issues/815
[issue-816]: https://github.com/AntoineToussaint/lazybox/issues/816
[issue-817]: https://github.com/AntoineToussaint/lazybox/issues/817
[issue-818]: https://github.com/AntoineToussaint/lazybox/issues/818
[remote]: remote-daemon-scoping.md
[spike]: desktop-spike.md
[runbook]: byo-remote-runbook.md
[dogfood-log]: desktop-dogfood-log.md
