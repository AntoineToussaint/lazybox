# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

The primary shipped interface is a keyboard-driven terminal UI. The client/daemon
architecture is intentionally shared with other clients, so durable product state
and commands must not be owned solely by the TUI.

## Users

Lazybox is for an individual software developer who starts the day with a personal
set of intentions, then works across GitHub pull requests, issues, Linear tickets,
local repositories, shells, and coding agents without wanting those source systems
to dictate the shape of the day.

## Product Purpose

Lazybox is a reactive work inbox and workspace manager. It brings incoming work and
user-authored work into one place, lets each unit of work acquire persistent agent
and shell sessions, and keeps enough state that the developer can leave and resume
later. Success means the developer can capture, choose, start, pause, complete, and
resume work without reconstructing context from provider tools or terminal history.

## Positioning

Lazybox treats work items as durable execution environments rather than links in a
task list: a PR, ticket, or personal intention can become a workspace containing
worktrees, terminals, agents, notes, and history.

## Operating Context

- The user begins the day by quickly capturing a newline-separated personal list.
- Capture must not require choosing a repository or creating a directory.
- Personal items appear as a dedicated Hopper section near the top of the sidebar,
  immediately below Focused, and participate in ordinary workspace navigation.
- Starting an agent or shell for a repo-less item triggers repository assignment at
  that moment; creating the item itself remains uninterrupted.
- Hopper state is initially local to the active Lazybox daemon and persists in the
  existing SQLite state database across restarts.

## Capabilities and Constraints

- A hopper item is a personal kind of Workspace, not a parallel to-do record that
  must later be converted into one.
- A fresh hopper workspace may have zero sessions and no on-disk directory.
- Repository assignment is optional at capture time and required before creating a
  repo-backed worktree, agent session, or shell session.
- Completing an item archives it while preserving its workspace, terminals, and
  history. Destructive deletion is a separate, explicitly confirmed operation.
- Linking a personal item to Linear or another upstream task is deliberately outside
  the first experiment, but the identity model must leave room for it.
- The daemon owns durable state and filesystem lifecycle; clients render and dispatch
  commands through the shared IPC contract.

## Evidence on Hand

- DESIGN.md describes the established client/daemon architecture and interaction
  model.
- crates/core/src/workspace.rs already defines a zero-session Workspace as a pure
  tracking row and a Session as one folder worktree.
- crates/store persists workspace JSON in the existing SQLite-backed Store.
- crates/tui/src/components/sidebar and crates/tui-core/src/inbox define the
  current Focused and grouped workspace hierarchy.
- No user research, usage metrics, or cross-device synchronization requirement has
  been supplied; future work must not fabricate them.

## Product Principles

- Capture first; organize only when organization becomes necessary.
- Personal intentions and provider-originated tasks are equal units of work.
- Persistence begins at capture, while filesystem cost begins at execution.
- Completion is reversible; destruction is exceptional and explicit.
- Keep durable behavior source-agnostic and client-agnostic.

## Accessibility & Inclusion

The hopper must preserve Lazybox's keyboard-first operation, visible focus, existing
key-remapping model, and non-mouse path for every action.
