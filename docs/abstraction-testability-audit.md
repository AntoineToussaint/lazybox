# Workspace-wide abstraction and testability audit (issue #662)

> Point-in-time review of all 17 workspace crates. File and line references
> describe the audit baseline (`b9a381b`); follow the linked child issues for
> implementation status as later refactors move those lines.

This review looked for current obstacles to testing and changing lazybox:
ambient IO with no injection point, decision logic coupled to stateful
orchestrators, duplicated cross-provider policy, and large state owners whose
cohesion had become unclear. It did not treat abstraction as a goal by itself.
Similar code was left alone when it was already easy to exercise or when a
shared layer would only anticipate hypothetical use.

## Method and scope

Five read-only passes covered the workspace by responsibility. Each finding
below was checked against the named path and ranked by the cost it currently
imposes on tests or changes.

| Pass | Crates |
| --- | --- |
| Foundation | `core`, `auth`, `store`, `config` |
| Execution and terminal | `git-ops`, `tui-term`, `libghostty-vt`, `libghostty-vt-sys` |
| Providers | `gh-provider`, `linear-provider`, `slack-provider` |
| Daemon | `ipc`, `agents`, `server` |
| Client | `tui-core`, `tui`, `tui-boot` |

## Headline

The workspace is already strong at its lower layers. `CredentialProvider`,
`Store`, `Agent`, `TaskProvider`, `TaskSource`, and the IPC transports are real
execution seams, not marker traits. Time and path-sensitive code commonly has
an explicit variant such as `time_ago_at` or `Config::load_from`, and tests
guard ambient environment access. The dependency rules also keep `core` and
`auth` independent and providers limited to the intended foundation crates.

The remaining high-cost seams cluster in four places:

- `server` combines unrelated registries in one process-wide configuration and
  leaves spawn and polling decisions inside two very large modules.
- `tui` has adopted pure `tui-core` resolvers unevenly, leaving broad decision
  surfaces reachable only through a fully mounted `Model`.
- GitHub, Linear, and Slack independently encode pagination, fetch coverage,
  HTTP-client policy, and rate-limit details.
- `git-ops` and `tui-term` own real processes at the same boundary where their
  most important decision logic lives.

No behavior bug was found that justified mixing a fix into this audit. The
deliverable is this evidence plus independently reviewable child issues.

## High-severity work

| Area | Evidence at the audit baseline | Boundary to extract | Tracking |
| --- | --- | --- | --- |
| Git execution | `git-ops/src/lib.rs:541` chooses a start point while calling Git throughout the same flow; stale classification is likewise tied to command execution | A `GitRunner` seam plus pure start-point and stale-state decisions | [#654](https://github.com/AntoineToussaint/lazybox/issues/654) |
| Terminal ingestion | `tui-term/src/session.rs:105` owns the PTY, channel, VT parser, render iterators, recent-output buffer, and ingestion timing in `TermSession` | A testable ingestion core separate from PTY ownership | [#655](https://github.com/AntoineToussaint/lazybox/issues/655) |
| Server registries | `server/src/lib.rs:289` stores dozens of terminal, polling, provider, spawn, and persistence fields on `ServerConfig` | Cohesive sub-registries with explicit ownership | [#656](https://github.com/AntoineToussaint/lazybox/issues/656) |
| Spawn planning | `server/src/spawn_handler.rs:717` begins a handler in a roughly 12k-line module where policy selection and effects are interleaved | A pure `SpawnPlan` followed by a narrower executor | [#657](https://github.com/AntoineToussaint/lazybox/issues/657) |
| Polling responsibilities | `server/src/polling/mod.rs` is roughly 9.8k lines and owns source fetches, upsert decisions, store writes, and workspace lifecycle | `sources`, `upsert`, and `workspace` modules with the tick left as coordinator | [#658](https://github.com/AntoineToussaint/lazybox/issues/658) |
| Provider pagination | `gh-provider/src/client.rs:230` and `linear-provider/src/lib.rs:316` define separate complete/partial outcomes around separate cursor loops | A shared pagination helper and core-owned coverage contract | [#659](https://github.com/AntoineToussaint/lazybox/issues/659) |
| TUI decisions | Dispatch, choice resolution, and key decisions were spread across `tui/src/realm/model/{dispatch,modals,keys}.rs`, so tests exercised them through `Model` | Pure resolvers in `tui-core`; `Model` applies returned effects | [#660](https://github.com/AntoineToussaint/lazybox/issues/660) |
| TUI state | `tui/src/realm/model/mod.rs:863` placed more than 70 fields from unrelated flows on `Model`; the compensating test module exceeded 14k lines | Cohesive sub-state with narrower mutation surfaces | [#661](https://github.com/AntoineToussaint/lazybox/issues/661) |

These are separate issues deliberately. Combining them would make structural
movement in one crate obscure the review and regression signal in another.

## Secondary findings

### Server

- **Medium-high — inject the loaded user configuration.** Production handlers
  read from disk independently through `Config::load()` in
  `spawn_handler.rs:743`, `agent_runs.rs:100`,
  `polling/handlers.rs:1360`, and several polling paths beginning at
  `polling/mod.rs:2836`. Supplying one loaded configuration through the server
  boundary would make handler tests deterministic and keep a request on one
  configuration snapshot.
- **Medium — inject provider mutation targets.** Mutation orchestration such as
  `polling/handlers.rs:270` (`handle_merge_pr`) constructs or reaches the real
  provider internally. A `*_with(provider)` boundary would let tests assert
  sequencing and error propagation with a fake `TaskProvider`.
- **Medium — consolidate already-duplicated pure helpers.** `strip_ansi` exists
  in `spawn_handler.rs:3960` and `chat.rs:860`; `expand_tilde` exists in
  `spawn_handler.rs:3172` and `polling/handlers.rs:1344`; path canonicalization
  has two local forms in `polling/handlers.rs`. These are current duplicates,
  not a request for a general utilities layer.
- **Low — split detection only if navigation remains costly.**
  `agents/src/detect.rs` is about 2.2k lines, but its logic is already pure and
  well exercised. A per-agent module split would improve ownership, not
  testability, so it ranks below the daemon seams.

### Providers

- **Medium — share concrete HTTP construction policy.**
  `linear-provider/src/lib.rs:150` and `slack-provider/src/lib.rs:69` build
  `reqwest::Client`s independently with different timeouts. This is one real
  policy surface worth centralizing without introducing a provider framework.
- **Medium — keep rate-limit data typed.** Slack converts a retry delay into an
  error string at `slack-provider/src/lib.rs:112` and parses
  `retry-after=N` back out at `:135`. A typed `RetryAfter` avoids a
  string-format contract between two layers.
- **Medium — remove repeated mutation preambles.** GitHub mutations repeatedly
  poll to obtain a PR or issue node id before executing the requested
  operation (`gh-provider/src/client.rs:3047` onward). Narrow
  `pr_node_id`/`issue_node_id` guards would isolate that prerequisite and its
  errors.
- **Low — keep URL parsing in one place.** `extract_repo_from_url` is duplicated
  at `gh-provider/src/graphql.rs:2660` and
  `gh-provider/src/mentions.rs:290`, with different invalid-input fallbacks.

### TUI and boot

- **Medium — finish moving catalog interpretation.**
  `tui/src/realm/model/keys.rs:1888` and `:1922` bridge catalog entries to
  runtime actions. That bridge belongs beside the catalog types in
  `tui-core::action`.
- **Medium — move key resolution behind a TUI-free scope.**
  `find_action_for_stroke`, `find_action_for_seq`, `seq_continuations`, and
  `section_rank` live at `tui/src/realm/model/helpers.rs:170-258` and take
  `PaneFocus`. A `tui-core`-owned `ResolutionScope`, mapped from `PaneFocus` at
  the client boundary, would make collision and precedence rules reusable
  pure logic.
- **Medium — separate setup probes from classification.**
  `tui-boot/src/setup_detect.rs:44`, `:110`, and `:192` combine subprocess or
  network probes with status classification and have no direct tests. A
  `probe()`/`classify()` split would mirror the tested decision boundary in
  the build guard.

### Foundation

- **Medium — give `TileTree` a focused module.**
  `core/src/workspace.rs:1301` starts the tile tree and its enums inside a
  roughly 3.2k-line workspace module. Moving the existing types and tests to
  `core/src/tile_tree.rs` would clarify ownership without changing the API.
- **Medium — split configuration by responsibility.**
  `config/src/lib.rs` is roughly 2.6k lines and mixes section types, defaults,
  validation, and file IO. Per-section modules plus `config/io.rs` would
  preserve the public configuration shape while isolating disk behavior.
- **Low — complete three small test seams.** Add a wrapper test for
  `core/src/time.rs:28` (`time_ago`), a deterministic
  `SessionId::from_uuid` constructor beside `core/src/workspace.rs:63`, and
  `Snippets::load_for_launch_dir_with(global_path, launch_dir)` beside
  `config/src/snippets.rs:879`, mirroring the existing path-injected snippet
  write helper.

### Git operations

- **Low — make lock ownership match manager ownership.** The worktree lock
  registry is a process-wide `OnceLock` at `git-ops/src/lib.rs:31` rather than
  state on `WorktreeManager` (`:197`), which makes manager isolation in tests
  incomplete.
- **Low — isolate process-group reaping.** `git-ops/src/lib.rs:2474` calls
  `libc::killpg` directly. A small `ProcessReaper` seam would let tests cover
  the arm/disarm lifecycle without signaling a real process group.

## Verified healthy boundaries

- `core` and `auth` have no internal crate dependency; `store` depends only on
  `core`, and the workspace dependency-rule tests enforce the intended
  layering.
- Provider implementations depend on the provider-neutral task and credential
  boundaries instead of daemon or TUI types.
- `CredentialProvider` (`auth/src/credential.rs:62`), `Store`
  (`store/src/traits.rs:120`), `Agent` (`agents/src/agent.rs:134`),
  `TaskProvider` (`core/src/provider.rs:203`), and `TaskSource`
  (`server/src/polling/mod.rs:819`) are useful injection points already.
- The structured agent-run path has an `AgentStreamSpawner` seam
  (`server/src/agent_stream.rs:622`), demonstrating the narrower shape other
  execution paths should follow.
- Library production code does not rely on `unwrap!` or `panic!` for ordinary
  control flow, and existing ambient environment tests serialize access.
- The vendored terminal bindings intentionally preserve their `!Send`/`!Sync`
  boundary; no abstraction change is warranted there.

The audit therefore recommends targeted seams and ownership splits, not a
workspace-wide framework. Each implementation should keep behavior stable and
add the unit tests made possible by its new boundary.
