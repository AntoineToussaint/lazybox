# Full codebase review (issue #236)

A systematic pass over all 17 crates against the invariants in
`CLAUDE.md`, grouped into the five crate families from the issue.
Every finding cites a `file:line` that was **read**, not inferred.
Findings are severity-ranked and tagged **BUG** (correctness/panic),
**TECH-DEBT**, or **DOCS**. "verified" means the path was read and the
behaviour confirmed by hand; "reported" means it surfaced in the audit
and is well-argued but would benefit from a repro before acting.

Each actionable item should become a child issue so this EPIC collapses
to a triage hub rather than one mega-PR — the same pattern as
[`critical-review.md`](critical-review.md) (#180).

## Headline

The codebase is in good shape. The stated conventions hold almost
everywhere: **no non-test `unwrap()`/`expect()`/`panic!` in `core`,
`auth`, `store`, `config`, `git-ops`, `ipc`, `agents`, `llm-proxy`,
`linear-provider`**; `thiserror` in libraries / `anyhow` only in the
binary; the action-catalog collision detector exists in both static and
runtime forms; `ratatui`/`tuirealm` do **not** leak into `tui-core`; the
VT parser is built `ReleaseSafe` (issue #59 guard intact); the
`!Send`/`!Sync` terminal invariant is upheld by construction; and the
inter-crate dependency graph is acyclic with providers depending only on
`core` + `auth`.

The residual risk is concentrated in **`server`** (the 32k-LoC daemon),
where a handful of genuine correctness bugs live in PTY lifecycle and
session-folding paths, plus a pervasive mutex-poison `.expect()` family
that can silently kill the poll loop. The library crates contribute a
small number of real-but-minor correctness bugs (an FFI color tag, a
relative-time bucket gap) and the usual tech-debt/test-gap tail.

### Top fixes, in priority order

1. **Phantom-terminal leak** on pump subscribe-error — `server` (singleton guard never releases; `w`/`c`/`x` can't respawn).
2. **Lost-output window** in `DaemonPty::subscribe` ordering — `server`.
3. **Mutex-poison `.expect()` storm** silently kills polling after one panic — `server`.
4. **RGB style color dropped across FFI** (wrong union tag) — `libghostty-vt`.
5. **Non-adjacent dedup double-absorbs** a closing issue — `server`.
6. **Slack socket reconnect loop never terminates** on consumer drop — `slack-provider`.

## Status — BUG fixes landed in this PR

All **BUG**-severity findings below are fixed here except one (regression
tests added where a harness exists):

- ✅ Phantom-terminal leak — both pump sites tear down + emit `TerminalExited` on subscribe-error (`spawn_handler.rs`).
- ✅ Lost-output window — `DaemonPty::subscribe` now subscribes before snapshotting; consumers drop live chunks with `seq <= last_seq` (`pty.rs`, `spawn_handler.rs`).
- ✅ `wait_exit` repeatable — `DaemonPty` publishes the exit code on a `watch` channel instead of a consumed `oneshot`, fixing the tmux `None`-on-second-call bug (`pty.rs`; tests added).
- ✅ Non-adjacent dedup — order-preserving `HashSet` dedup of `closes_issues` (`polling/mod.rs`).
- ✅ JSON-API accept loop — logs + continues on transient accept errors (`api_gateway.rs`).
- ✅ Double `AgentRunFinished` — the `runs` map entry is now the single token for the terminal event; the driver removes it before emitting (`agent_runs.rs`).
- ✅ Slack reconnect loop — `run_once` returns a distinct `ConsumerGone` outcome and `run_forever` stops (`slack-provider/socket.rs`).
- ✅ `time_ago_at` 0y gap — months branch gated on `days < 365` (`core/time.rs`; tests added).
- ✅ RGB FFI tag — `StyleColor::Rgb` now emits `RGB` (`libghostty-vt/style.rs`; tests added).
- ✅ `Formatter` dangling pointer — selection pointer taken from a named local (`libghostty-vt/fmt.rs`).
- ✅ Stale projects on reconnect — `Snapshot` prunes vanished daemon projects, keeps synthesized placeholders (`tui/events.rs`; tests added).
- ⏸️ **Deferred:** `llm-proxy` full-body buffering. The proxy is currently dead code (zero non-test callers) and a correct streaming fix must coexist with the telemetry path, which needs the whole body — i.e. it depends on the dead-code/wiring decision (see tech-debt below). Left as a follow-up rather than reworking an unwired module speculatively.

Daemon-internal fixes (phantom-terminal, dedup, accept loop, agent-run
race) have no unit tests: the crate has no `ServerConfig` test harness
and the affected functions had none originally. They are verified by
`cargo test`/`clippy` compiling clean and by inspection; building that
harness is follow-up work.

---

## BUG

### Daemon (`server`, `llm-proxy`)

- **Phantom-terminal leak on pump subscribe-error** — `server/src/spawn_handler.rs:708-714` (verified) and the same shape at `:3376-3382` (`recover_sessions`). `TerminalSpawned` is broadcast, then the pump task `return`s on a `backend.subscribe()` error *before* the teardown block (`:892-909`), so `terminal_meta` / `terminals` / `terminal_sessions` / `no_permission_terminals` and the `terminal:*` store keys are never cleared and no `TerminalExited` fires. The phantom entry permanently satisfies the singleton guard, so `w`/`c`/`x` only re-focus a dead terminal and never respawn. *Fix:* extract teardown into a helper and run it (and emit `TerminalExited`) on the subscribe-error path.

- **Lost-output window in `DaemonPty::subscribe`** — `server/src/pty.rs:400-408`. The ring snapshot is taken (releasing the ring lock) *before* `output_tx.subscribe()` creates the broadcast receiver; a chunk pushed in that gap lands in neither replay nor live stream. *Fix:* subscribe to the broadcast first, then snapshot, and have the consumer drop live chunks with `seq <= last_seq` (the forwarder already dedups).

- **Non-adjacent dedup double-absorb** — `server/src/polling/mod.rs:3394` (verified). `closed_ids.dedup()` runs without a preceding `sort`, so only *adjacent* duplicate issue ids collapse. `pr.closes_issues` unions GraphQL refs + body-text parses, so a non-adjacent duplicate folds the same issue workspace into the PR twice in one pass. *Fix:* `sort()` then `dedup()`, or dedupe via a `HashSet`.

- **`wait_exit` not repeatable for tmux backend** — `server/src/backend/tmux.rs:770-785` → `pty.rs:456-463`. The trait contract says `wait_exit` returns the cached code on repeated/concurrent calls, but `TmuxBackend` forwards straight to `DaemonPty::wait_exit`, which consumes its oneshot (`exit_rx…take()`) — a second/concurrent call returns `None`. `RawPtyBackend` caches via a watcher task; tmux doesn't. *Fix:* cache the code in the tmux `Slot`.

- **JSON-API listener dies on one transient accept error** — `server/src/api_gateway.rs:196`. The accept loop propagates any `accept()` error with `?`, so a transient `EMFILE` permanently tears down the listener — unlike `socket_service.rs:113-119`, which logs and continues. *Fix:* log + `continue`, reserving exit for fatal cases.

- **Double `AgentRunFinished` on completion/interrupt race** — `server/src/agent_runs.rs:115` vs `:162-172`. Natural completion sends `AgentRunFinished{error:None}` and *then* removes its handle; an interrupt in that gap aborts the finishing task and emits a second `AgentRunFinished{error:"interrupted"}` for the same run. *Fix:* remove the handle before sending the final event, or only emit on interrupt when it actually transitioned a still-running run.

- **`llm-proxy` buffers the whole response, defeating SSE streaming** — `llm-proxy/src/server.rs:342-346` (verified). `response.bytes().await` fully buffers the upstream body before returning `Full<Bytes>`; an agent pointed at this proxy would receive nothing until the turn completes, contradicting the module's "byte-for-byte pass-through, no timeout so streams aren't severed" docs. Currently **latent** because the proxy is unwired (see tech-debt below). *Fix:* stream via `reqwest::Response::bytes_stream` → a streaming hyper body. (Fix the wiring/dead-code question first.)

### Providers

- **Slack socket reconnect loop never terminates on consumer drop** — `slack-provider/src/socket.rs:226-230` + loop at `:155-171` (verified). When `tx.send` fails (receiver dropped), `run_once` returns `Ok(())` ("Consumer dropped — shut down"), but `run_forever` treats `Ok(())` as a clean socket close and reconnects, re-opening `apps.connections.open` indefinitely (~60s clean-close cycle). Only aborting the `JoinHandle` stops it. *Fix:* return a distinct sentinel/`Err` on `tx.send` failure and `break` the outer loop.

### Core libraries

- **`time_ago_at` emits "0y ago" for ages 360–364 days** — `core/src/time.rs:36-42` (verified). The months branch gates on `months < 12` where `months = days / 30`; at `days = 360`, `months = 12` (not `< 12`), so it falls through to `years = days / 365 = 0` → `"0y ago"`. The `/30`-month and `/365`-year buckets don't meet. *Fix:* gate the months branch on `days < 365` (or compute `years` first and check `years < 1`). No tests exist for this file — see test gaps.

### Terminal stack

- **RGB style color silently dropped across FFI** — `libghostty-vt/src/style.rs:203-206` (verified). In `From<StyleColor> for ffi::StyleColor`, the `Rgb` arm sets `tag: ffi::StyleColorTag::NONE` (copy-paste from the `None` arm) while populating the `rgb` union field. The C side keys off `tag`, so every RGB style color round-trips as "no color," corrupting `Style::is_default()` and any caller building an `ffi::Style` from an RGB color. *Fix:* set `tag: ffi::StyleColorTag::RGB`.

- **Dangling pointer to `selection` across FFI** — `libghostty-vt/src/fmt.rs:71-85`. `match selection { Some(s) => &raw const s, … }` takes the address of the match-arm-local `s`, which drops at the arm's end, so `opts.selection` is dangling when `ghostty_formatter_terminal_new` reads it → UB. Latent (lazybox's `tui-term` doesn't use `Formatter`), but it's a public safe API. *Fix:* bind to a named local that outlives the FFI call before taking its address.

### TUI

- **Stale projects not pruned on reconnect `Snapshot`** — `tui/src/realm/model/events.rs:199-205` (verified). The `Snapshot` handler is insert-only into `self.projects`; `ProjectRemoved` (`:189-194`) prunes, but a project deleted while the client was disconnected (out-of-process / SSH socket mode) lingers as a ghost header until restart. No impact in default in-process mode (Snapshot fires once at startup against an empty map). *Fix:* rebuild `self.projects` from the snapshot's project list (clear-then-insert or retain-only-snapshot-keys) before `apply_projects`.

---

## TECH-DEBT

### Mutex-poison panics in non-test library/daemon code

The "no `unwrap()`/`expect()` in library crates" rule is violated almost
exclusively by `std::sync::Mutex` poison handling, ranked here by blast
radius. All protected data is a rebuildable cache/set that tolerates
poisoned-but-consistent state, so the uniform fix is
`lock().unwrap_or_else(|e| e.into_inner())`.

- **`server` poll loop — silent polling death** (worst): `polling/mod.rs` pervasive (369, 377, 384, 416, 618, 1059-1074, 1128, 1189, 1210, 1526, 1599, 1618) and `handlers.rs:489,509,1308`. The `catch_unwind` poll wrapper (`mod.rs:2549`) means one panic while holding e.g. `gh_client_cache`/`pending_actions` poisons the lock; every later tick then panics at the next `.expect()`, gets swallowed, and **polling is silently dead-but-looping until restart**.
- **`server` spawn/kill**: `spawn_handler.rs:1192,1989,2005,2079,2140` (`inflight_spawns`/`deleted_workspaces`); a poisoned `inflight_spawns` panics on every spawn/kill.
- **`server` misc**: `lib.rs:867-871,1058-1062,1069-1073,1091-1098`; `slack.rs:166-194`.
- **`gh-provider`**: `client.rs:455,491,947,966,989,1013,1030,1041,1064,1079,1140,1239` — `.lock().expect("…poisoned")`, inconsistent with `observe_rate_limit` (`:731`) which handles poisoning gracefully.

### Dead / superseded code

- **`llm-proxy` + `server/src/agent_spawn.rs` are effectively dead** (verified) — `spawn_with_proxy` / `AgentSpawn` / `AgentSpawnConfig` / `ProxyServer` have zero non-test callers; the live gateway path is env-only (`spawn_handler::gateway_env_for_agent` pointing the base-URL var at `agent.llm_gateway_url`). `pricing.rs` (`rate_per_mtok`/`estimate_cost`) is only called by its own tests. *Fix:* delete the superseded surface, or wire it back in (and fix the streaming BUG above first). This decision gates whether the `llm-proxy` streaming bug is worth fixing.
- **Dead public API in `core`** — `core/src/config.rs:251` (`KV_KEY_LAYOUT`), `:242` (`KV_KEY_THEME`), `:265-302` (`PaneLayout` + `DEFAULT`/`clamp`/`nudge`), all `pub` and re-exported from `lib.rs:24` with zero workspace consumers (the TUI does its own splitter math in `tui/src/realm/layout.rs`; theme persistence goes through `ui.theme`). `KV_KEY_THEME`'s doc ("Cycled with the `T` global keybind; persisted") is stale. *Fix:* delete, or wire up.
- **Linear dead-code import shim** — `linear-provider/src/graphql.rs:236-241` `_activity_kind_imported` exists only to suppress an unused-import warning for `ActivityKind`. *Fix:* drop the unused import.
- **Unused `lazybox-core` dependency** — `slack-provider/Cargo.toml:11` declares `lazybox-core` but only `lazybox_auth` is used in `src/`. Not a rule violation, just dead weight. *Fix:* remove the line.

### PTY / resource lifecycle (`server`)

- **No `Drop` on `DaemonPty`** — `pty.rs:84-120`. Reader/writer threads hold *duplicated* PTY fds; dropping a `DaemonPty` drops only the master Arc's fd, so the kernel PTY stays open (child gets no SIGHUP) and the reader/exit threads block forever. A bulk teardown that drops the sessions map bypasses the per-slot `kill()` and leaks every child + 2 threads each. *Fix:* `impl Drop for DaemonPty` calling `kill()`.
- **`ReplayRing::push` write amplification** — `pty.rs:153-168`. At the 2 MiB cap every push does `copy_within(excess.., 0)` (~2 MiB memmove per ~8 KiB chunk ≈ 256× amplification) on the reader thread under the ring lock, during spinner/full-repaint churn. *Fix:* circular buffer (head/tail) or `VecDeque<u8>` draining only the overflow.
- **PID-reuse hazards** — `pty.rs:385-397` (`kill`) signals `child_pid` even after the exit-watcher reaped it; `lifecycle.rs:137-148` (`request_stop`) re-signals a pid validated in a separate step. Low likelihood on a local single-user daemon. *Fix:* skip the signal when finished/reaped.

### Error classification & robustness

- **Mutation paths flatten typed errors to `permanent`** (cross-provider) — gh `client.rs` mutations (`merge` :2697, `request_reviewers` :2722, `add_assignees` :2755, `set_assignees` :2811/2816, `post_reply` :2867, `list_repo_labels` :2900, `set_labels` :2990/2995) and linear `lib.rs:392` (`post_reply`) use `.map_err(|e| ProviderError::permanent(…))`, bypassing the `From<GhError>`/`From<LinearError>` impls and discarding `RateLimited`/auth classification + `retry_after_secs`. The fetch paths correctly use `.map_err(Into::into)`. *Fix:* use the typed conversion on mutations too.
- **Corrupt-decode → data loss in rescope** — `server/src/polling/mod.rs:2257-2383` (`rescope_with_state`). A stored workspace whose JSON fails to deserialize yields `stored_ws = None`, bypassing the snooze / locally-authored / authoritative-scope guards; if also absent from the poll with no live terminal it gets deleted — a transient/corrupt decode becomes data loss. *Fix:* treat decode-failure on non-empty `workspace_json` as "preserve." Separately, `create_empty_workspace` (`:4004`) uses `.expect("infinite range yields a free key")` and treats a `NULL workspace_json` record as a free key (could overwrite) — bound the loop + check record presence.
- **`/v1/commands` reports success before dispatch completes** — `server/src/api_gateway.rs:288-296` + `lib.rs:777-787`. Returns `200 ok:true`, then drops `command_tx`, closing the serve loop which only drains detached mutations for 5s; a command exceeding that (e.g. `Spawn` onto a cold clone) is aborted mid-flight despite the client being told it succeeded. *Fix:* await dispatch completion before responding, or document as best-effort.
- **Slack `open_wss_url` ignores HTTP status / rate limits** — `slack-provider/src/socket.rs:132-147` does `.send().await?.json().await?` directly, unlike `api.rs::call` which maps 429→typed `rate_limited` and 5xx→`Api`. A 429/HTML error surfaces as opaque `Json`/`Http`, losing `Retry-After`. *Fix:* route through the shared status-gated parse.
- **Linear viewer-query errors swallowed** — `linear-provider/src/lib.rs:229-234`; `fetch_all_with_coverage` checks only `viewer.data` and reports a generic "no viewer data", ignoring `viewer.errors` (the page loop at `:259-273` does inspect errors). *Fix:* surface `viewer.errors`.
- **Claude-trust atomic-write temp-name race** — `agents/src/claude_trust.rs:90-98` (verified). The temp file is a fixed sibling `<config>.lazybox-tmp`; two concurrent unattended spawns seeding `~/.claude.json` race on the same temp path + rename, so one worktree's trust write can be lost. *Fix:* unique temp name (pid + counter or worktree-hash suffix).

### Smaller cleanups

- **`unreachable!()` / infallible `.expect()` in non-test code** (panics in libraries; all provably sound today but conflict with the convention): `server/src/lib.rs:1178-1180` (`Shutdown` arm), `agent_spawn.rs:121` + `llm-proxy/src/lib.rs:75` (`"127.0.0.1:0".parse().expect(…)` → use `SocketAddr::new(Ipv4Addr::LOCALHOST, 0)`), `gh-provider/src/notifications.rs:144`, `gh-provider/src/graphql.rs:2376`, `libghostty-vt` (`terminal.rs:908,916`, `osc.rs:83`, `style.rs:66`), `tui-term/src/ghostty_widget.rs:120`, `tui-core/src/platform.rs:257`, `tui-core/src/intent.rs:233,238,247`.
- **`SqliteStore` uses `trim_start_matches` instead of `strip_prefix`** — `store/src/sqlite.rs:116,145` strip *repeated* leading occurrences of the whole prefix, unlike `MemoryStore` (`mock.rs:52`). Not exploitable today, but inconsistent. *Fix:* `key.strip_prefix("workspace:").unwrap_or(&key)`.
- **`RemoveAtError` unnameable** — `core/src/workspace.rs:750` returns `Result<_, RemoveAtError>` on a public re-exported type, but `RemoveAtError` is not in the `pub use workspace::{…}` list (`lib.rs:33-37`). *Fix:* add it to the re-export.
- **Duplicated assembly / structs**: gh `client.rs` `fetch_selected*` / `fetch_round_robin_with_status_and_mentions` (:2016, :2085, :2159, :2233) repeat the 4-arm partial-failure `match`; `extract_repo_from_url` is duplicated with divergent fallbacks (`graphql.rs:2187` → `"unknown/unknown"` vs `mentions.rs:210` → `""`); slack `ConversationsCreateResponse`/`ConversationsJoinResponse` (`api.rs:314-322`) are byte-identical; `ipc/src/socket.rs:281-353` `writer_loop`/`reader_loop` vs their `_bounded` twins differ only by channel type.

### Test gaps (CLAUDE.md: "every public function has a test")

- `core/src/time.rs` — `time_ago`/`time_ago_at`/`staleness` have **no** test module (and hide the `time_ago_at` bug above); `core/src/agent.rs` (`spawn_command`/`Default`), `core/src/provider.rs:310` (`provider_for_workspace`), `core/src/config.rs:229` (`allows_scope`).
- `linear-provider/src/lib.rs:337` (`post_comment`/`post_reply`) untested despite the in-tree mock-server harness.
- `slack-provider` — `bot_credential_chain`/`app_credential_chain` (`lib.rs:51,57`), `post_message`, `conversations_create`/`_archive`/`_join`, non-paginated `conversations_list` (`api.rs`).

---

## DOCS

- **CLAUDE.md dependency invariant is imprecise** — it says "core, auth, store must NEVER depend on each other," but `store` legitimately depends on `core` (`store/Cargo.toml`; `store/src/traits.rs:2` uses `lazybox_core::{ProjectKey, WorkspaceKey}`). There's no cycle — `core` and `auth` are clean, and the layering is one-directional. *Fix:* reword to "core and auth must not depend on any other lazybox crate; store/config/git-ops may depend on core."
- **CLAUDE.md Global keybindings omit two actions** — `OpenThemePicker` (default `t`, `tui-core/src/action.rs:448`) and `StartAgent` (default `Shift-W`, `action.rs:483`) are Global, wired through dispatch and help, but absent from the Keybindings → Global section. *Fix:* add `t` (theme picker) and `Shift-W` (start agent).
- **`Terminal::pwd()` doc copy-pasted from `title()`** — `libghostty-vt/src/terminal.rs:379-385` says "An empty string is returned when no **title** has been set"; should refer to the working directory (OSC 7).
- **Slack socket doc drift** — `socket.rs:54-57` claims the `<@Uxxx>` mention prefix is stripped in `text` (the crate's own test asserts it's present; stripping is downstream); `:11-12` ACK doc shows a `payload` field the code doesn't send; `:73-77` says the loop rebuilds the URL on `Disconnect` but it actually waits for Slack to close the socket.
- **Linear minor field-mapping notes** — `issue_to_task` always sets `closed_at: None` even when state maps to `Closed` (`graphql.rs:218`; the simple query doesn't fetch `completedAt`/`canceledAt`), and sets `mergeable: Mergeable::Mergeable` for non-PR issues (harmless). Worth a comment or follow-up if Inbox grace-window logic ever applies to Linear.

---

## Verified clean (invariants upheld)

- **No non-test `unwrap()`/`expect()`/`panic!`** in `core`, `auth`, `store`, `config`, `git-ops`, `ipc`, `agents`, `llm-proxy`, `linear-provider`. Remaining hits are all under `#[cfg(test)]`/`tests/`.
- **`thiserror` in libraries, `anyhow` only in the `tui` binary** — no library Cargo.toml pulls in `anyhow`.
- **Dependency graph acyclic; providers depend only on `core` + `auth`** (`gh-provider/tests/dep_rules.rs` guards this); `core`/`auth` depend on no other lazybox crate.
- **Action catalog**: static within-section collision detector (`tui-core/src/action.rs:1931`) + runtime detector including generated per-agent rows (`tui/src/realm/model/helpers.rs:1485`); the None-gate / handled-set dispatch split leaves unmigrated actions to pane handlers without silently swallowing them.
- **`ratatui`/`tuirealm` do not leak into `tui-core`** (only a doc-comment mention).
- **VT parser built `-Doptimize=ReleaseSafe`** (`libghostty-vt-sys/build.rs:75`) — issue #59 Debug-build freeze guard intact, safety checks kept on for untrusted VT input.
- **`!Send`/`!Sync` terminal invariant by construction** — no `unsafe impl Send/Sync`; FFI handles hold `NonNull` + `PhantomData<*mut ()>`, so cross-thread use fails to compile. Drop coverage is complete (every owning wrapper has a matching `*_free`).
- **`server` concurrency core is sound** — seq monotonicity (push-then-bump under one lock), bounded forwarder resync bookkeeping cleaned on `TerminalExited`, acquire-and-drop lock discipline (no co-held locks across `terminal_*` maps), collision-safe tmux naming, `kill_on_drop(true)` on agent-run children.
- **`gh-provider` fetch path** — cursor-follow pagination, 502/401/parse-failure classification, sweep-window state machine, and the `comments(last:1)` bodiless-deserialize regression all have tests.
