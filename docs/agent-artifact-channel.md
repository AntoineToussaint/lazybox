# Rendering agent output: the artifact channel

## Decision

Do not override the terminal emulator. Keep `crates/tui-term` a VT and keep
the PTY byte stream forwarded verbatim.

Give the agent a second, *structured* way to hand lazybox something to
render — a typed artifact — and render it in a surface lazybox already
owns. Ship it narrowly: markdown first, opened in the existing reader
modal.

Carry artifacts on **the filesystem**, in a spool directory under the
worktree's `.lazybox/`, noticed by the daemon. Not an OSC sequence, and
not a path announced on stdout. Both of those ride the terminal stream —
not because lazybox could not parse them back out (it already does that
for OSC 52) but because they are intercepted in the client rather than
the daemon, and because they need an agent affordance that writing a file
does not.

## What is true in the tree today

Verified against the source rather than assumed, because the shape of the
answer depends on it:

- **The VT renders text and nothing else.** `crates/tui-term/src` is three
  files. `TermSession` exposes a cell grid (`render_data`), a recent-bytes
  tail (`recent_output`) and mode predicates (`in_alternate_screen`,
  `is_mouse_tracking`). There is no sixel and no Kitty *graphics* protocol
  anywhere; the Kitty support that does exist is the *keyboard* protocol,
  a different thing that shares a name.
- **One OSC sequence is already lifted out of the agent's stream**, which
  corrects a premise in #1818. `terminal_stack` scans each chunk for OSC 52
  clipboard-set sequences (`osc52_scan`) and forwards them verbatim to the
  host terminal (`forward_osc52`), buffering a sequence split across read
  boundaries in `osc52_carry` under a 4 MiB cap. So "lazybox cannot pick a
  sequence out of the byte stream" is not true, and the artifact design
  should not lean on it being true.
- **lazybox already owns a good markdown renderer.**
  `components::markdown_doc::render_markdown(src, width, theme)` parses
  CommonMark + GFM through `pulldown-cmark` and returns a `RenderedDoc`
  with click-mapped links. `MarkdownModal` wraps it, and
  `Model::mount_description_modal(title, body, ask_subject)` mounts it. A
  markdown artifact therefore needs to produce exactly two things — a
  title and a body — to reach a finished reader. It is not quite a
  drop-in, in one direction each way. Against: that function early-returns
  when `Id::DescriptionModal` is already top of the modal stack, so an
  artifact opened while a task description is up would silently do
  nothing. For: `Id::consumes_scroll` whitelists `DescriptionModal` alone
  for wheel events, so a *new* modal id would silently lose mouse scroll.
  Reuse the id and handle the stacking; do not mint a fresh one.
- **Two structured agent → daemon channels already exist, and neither
  touches the PTY.**
  - *Hooks.* The agent runs `lazybox hook-ingest --backend-key K`, whose
    payload `parse_claude_hook` normalizes and
    `lifecycle::ingest_hook_from_stdio` ingests. Claude is wired through a
    generated settings file (`Agent::build_hook_settings`), Codex through
    spawn-argv `-c hooks.*` overrides (`Agent::hook_command_args`); the
    payloads are wire-compatible, so both parse unchanged. This channel is
    already load-bearing twice over: it carries the `SessionStart` capability
    text (`lazybox_session_context`) and the bidirectional `PreToolUse`
    read intercept, where the daemon returns a decision the agent waits on.
  - *MCP.* `crates/server/src/mcp.rs` is a daemon-hosted MCP server with
    identity implicit from a per-session bearer token, so no tool takes a
    "who am I" argument.
- **A per-workspace, agent-visible, daemon-owned, git-excluded directory
  already exists.** `.lazybox/task.json` (`TASK_FILE_RELATIVE_PATH`) is
  written into the worktree at spawn by `task_cache::write_record_file`,
  which calls `exclude_record_file` to append the pattern to
  `info/exclude` in the git *common* dir. The agent is told about the file
  in its session context and reads it directly.

That last fact is the one that settles the design. The hard part of an
artifact channel — a place both sides agree on, inside the worktree,
invisible to `git status`, surviving respawn — is already built and in
production for a different payload.

## Why overriding the VT is the wrong shape

The issue's four reasons hold up, and the source sharpens the first:

1. **The agent is itself a full-screen TUI.** `TermSession::in_alternate_screen`
   exists precisely because agents take the alternate screen and repaint it.
   There is no stable document in that stream to parse — markdown scraped
   out of a repainting VT would mis-parse on every redraw.
2. **The grid is the contract.** Scrollback, search, selection, copy, replay
   and the #1547 size-stamp machinery all assume a cell grid of known
   dimensions. A pane that sometimes contains an image of unknown cell
   height breaks the row arithmetic those depend on.
3. **Images need a cooperating host.** lazybox draws into *another*
   terminal, so an inline image means re-emitting Kitty graphics or sixel
   at the right cell offset and hoping the host obliges. That is a
   per-host capability matrix in the hot render path.
4. **A website is not a terminal artifact.** Rendering HTML in a cell grid
   is a browser. That belongs in `apps/desktop`, not `tui-term`.

## The transport: why the announcement goes away

The issue offered two forms and asked which survives a repainting TUI:
an OSC sequence lazybox claims, or a file under the session dir whose path
the agent **names on stdout**. It called this the open question that needs
measuring rather than reasoning about.

The OSC 52 precedent above means the mechanical objection does not hold —
lazybox demonstrably can pull a chosen sequence out of the stream, carry
included. Three things are still wrong with putting the announcement
there, and none of them is about parsing difficulty:

1. **It is intercepted on the wrong side.** `forward_osc52` runs in
   `crates/tui`, the *client*, and it works because clipboard is a pure
   pass-through: bytes in, bytes out to the host, no state touched. An
   artifact is the opposite — it has to be attached to workspace state,
   which lives in the daemon. A client-side scanner would have to ship
   the payload back across the socket to the process that already had it.
2. **The agent has to be able to emit it, and that is the part nobody has
   measured.** OSC 52 arrives from inner programs — a shell, vim, tmux —
   that write raw bytes to their own stdout. Claude Code and Codex are
   full-screen frameworks that own the screen; neither exposes "emit this
   escape sequence verbatim" as an affordance. Writing a file, by
   contrast, is something every agent can already do today with the tools
   it ships with. The transport that needs no new agent feature wins on
   availability alone.
3. **It competes with the render path.** A markdown document is not a
   clipboard blob; pushing one through the PTY puts payload bytes in the
   same stream as the repaint, under a carry cap, for a consumer that has
   to buffer it.

So: drop the announcement rather than choose between its two forms. The
daemon already owns `.lazybox/` in the worktree and already writes into
it. If the artifact spool is a known subdirectory, the daemon can find
it — the agent writes a file, the daemon notices. No sentinel,
no escape sequence, no parsing of anything the agent painted. "Survives a
repaint" stops being a question because nothing entered the VT.

This also makes the *transport* agent-agnostic, which matters more than it
first appears — and cuts differently than it first appears:

| Channel | Claude | Codex | Cursor | GenericCli |
| --- | --- | --- | --- | --- |
| Spool directory | yes | yes | yes | yes |
| Hook payload | yes | yes | no | no |
| MCP tool | yes | no | no | no |
| *Told it exists today* | *yes* | *no* | *no* | *no* |

`Agent::supports_mcp_config` defaults to `false` and only Claude overrides
it, so the MCP server reaches Claude alone today. Hooks reach Claude and
Codex. Writing a file needs no agent capability whatsoever — it is the only
one of the three that works for every agent lazybox can spawn, including
`GenericCli`, whose whole point is that lazybox knows nothing about it.

Read the last row before quoting the first. *Writing* to the spool needs no
capability; *learning that the spool exists* is a separate channel with its
own, currently narrower, reach — today the session-context briefing is
Claude-only. The transport is agent-agnostic; the feature is not agent-
agnostic until the announcement is solved, which is its own problem below
and not a detail of this one.

MCP is still worth adding later, but as **ergonomics over the same
contract**, not as a competing transport: a `post_artifact` tool whose
implementation writes the same spool file. An agent with the tool gets a
typed call; an agent without it writes a file. One contract, one renderer,
two front doors.

A hook payload cannot serve as the transport at all. A hook's fields are
fixed by the agent — `parse_claude_hook` reads `hook_event_name`,
`session_id`, `cwd`, `tool_name`, `notification` — and there is no slot to
put an artifact in. Its only role here would be as a *nudge* that a write
happened (a `PostToolUse` on a `Write`), which reading the spool already
gives us without depending on which agent is running.

## The open questions, answered

**Does an artifact belong to the session, the workspace, or the turn?**
The workspace. The filesystem settles this: the spool lives in the
worktree, and a respawn reuses the same worktree path, so artifacts
outlive the session that wrote them exactly as `task.json` does. This is
also the only answer consistent with the repo's standing invariant that
the tracker record *is* the workspace — an issue and the PR that closes it
share one row, and they should share one artifact list. Session scope
would drop artifacts on respawn; turn scope has no durable representation
anywhere in the system.

**Should the output contract mention artifacts?** No — it stays
deliberately ignorant. The contract (`config::snippets::output_contract`)
is appended at delivery to every built-in snippet and has to keep working
headless, over SSH, on a phone. Artifacts need a worktree and a running
daemon. Teaching the contract about a channel that is not always present
would make every snippet's closing summary conditional on something it
cannot check.

**Where the announcement goes is a separate, harder question, and the
obvious answer is wrong.** `lazybox_session_context` looks like the home
for it: that text exists to tell an agent "what lazybox lets you do beyond
plain `git`/`gh`" and it already names `.lazybox/task.json`. But it does
not reach every agent. `session_context_to_emit` returns `None` unless the
hook is `SessionStart` *and* `--emit-session-context` is in argv — a marker
Claude's hook command carries and Codex's deliberately omits, so Codex's
`SessionStart` is a documented no-op. Cursor and `GenericCli` override
neither `Agent::build_hook_settings` nor `Agent::hook_command_args`, so
they have no hook path at all.

Putting the announcement there unmodified would hand the most universal
transport a Claude-only announcement channel — narrower than hooks, and no
broader than the MCP tool it was chosen over. Codex, Cursor and
`GenericCli` could each write to the spool and would never learn it
exists.

The Codex half is a known gap rather than an oversight, and it comes with
its own blocker: `hook_command` notes that only the settings-file path
carries the marker "since it is unverified whether Codex surfaces a hook's
stdout as context", with a `TODO(codex)` to route the same text through a
per-launch Codex configuration once that is confirmed. So an artifact
announcement for Codex is downstream of a measurement nobody has taken.

So announcement is a per-agent-capability problem in its own right, and
lazybox already has the machinery for exactly that: the emission is gated
on markers the daemon adds per spawn, and `lazybox_session_context_with_mcp`
is the precedent for a capability paragraph that appears only for sessions
actually wired to it. An artifact announcement follows that pattern — its
own marker, added for spawns whose agent lazybox can brief — and the agents
with no hook path need a non-hook carrier. Two candidates, to be settled by
the slice that adds the announcement rather than here: the spawn-time work
prompt, or a short `README` written into the spool directory itself, which
is discoverable by an agent that goes looking and costs one file. Whichever
it picks, that slice owes a check of the carrier's actual per-agent reach —
assuming reach is the mistake this section exists to record.

The plain-text closing summary stays the always-works path; an artifact is
what an agent reaches for when it has something a paragraph genuinely
cannot carry.

## Slices

1. **Markdown, spool directory, existing modal.** The daemon picks up
   `.lazybox/artifacts/` (watch or poll — it decides, see below), excludes
   it from git the way `write_record_file` excludes `task.json` and in the
   same pass, attaches what it finds to the workspace, and the
   TUI opens a markdown artifact through `mount_description_modal`. Nearly
   all of the value, and it reuses a renderer and a modal that already
   ship.
2. **`post_artifact` MCP tool** — sugar over slice 1's contract for the
   agent that can call it.
3. **Mermaid / graph** — laid out to cells in the TUI (legible and
   theme-safe), to an image in the desktop shell.
4. **Image** — gated on host capability, with a placeholder card and "open
   externally" where the host does not advertise support.
5. **Website** — never inline in the TUI; the browser, or a web view in
   `apps/desktop`.

## Not verified

- Whether a directory watch or a poll is the right mechanism for the
  daemon side, and at what interval. Slice 1 decides that against the
  polling tiers in `crates/server`; this document says only "noticed", and
  deliberately does not pick. The decision is not cost-free either way:
  there is no filesystem-watch dependency anywhere in the workspace today
  (the `Notify` in `crates/server` is `tokio::sync::Notify`, an async
  primitive, not an fs watcher), so a watch means a new third-party crate
  through `cargo-deny` and `machete` plus a cross-platform inotify /
  FSEvents surface, while polling is the idiom the daemon already runs.
- Whether `.git/info/exclude` in the *common* dir is the right place for a
  per-worktree artifact pattern. It is what `task.json` does, and the
  pattern is worktree-relative. The sharp edge is that
  `task_cache::exclude_record_file` is a read-check-append-rename with no
  lock, which is harmless while it writes exactly one idempotent pattern
  and stops being harmless with two. Concretely: if the artifacts pattern
  is ever added from a *different* call site than `write_record_file`, a
  spawn that read the pre-artifacts contents can rename its version over
  the one that just added it — the pattern is lost, artifacts dirty the
  worktree, and the dirty-worktree delete refusal (#1805) fires on a
  workspace the user never dirtied. So the constraint on slice 1 is that
  both patterns are written by the same call site, in one pass; that is a
  requirement, not a caution.
- No measurement of OSC or stdout survival under either agent was
  performed. The design removes the need for one, but the prior question —
  whether Claude Code or Codex can be made to emit a chosen escape
  sequence verbatim at all — is asserted from their lack of a documented
  affordance, not from a test. Anyone reviving the inline form owes both
  measurements.
- The claim that artifacts survive respawn rests on a respawn reusing the
  same worktree path, which is what `task_cache` relies on for
  `task.json`. It was read from that code, not exercised here.
