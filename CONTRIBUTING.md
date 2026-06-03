# Contributing to lazybox

Glad you're here. A few ground rules so the codebase stays maintainable.

## Build + run

```sh
cargo build                       # first build compiles Zig/ghostty + SQLite (~30s)
cargo run -p lazybox-tui            # uses `gh auth token` automatically
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Logs go to `/tmp/lazybox.log`. Persistent state lives in `~/.lazybox/v2/state.db`.

## Standing rules

These are enforced in code review:

1. **Files over 1500 lines are unacceptable.** Split aggressively into sibling files. `model.rs` was 4626 lines, now lives in 8 focused files under `crates/tui/src/realm/model/`. Same shape applies elsewhere.
2. **No `unwrap()` in library crates.** `thiserror` for error types in `crates/{core,auth,events,store,config,git-ops,gh-provider,linear-provider,tui-term,tui-core,ipc,agents,llm-proxy}`. `anyhow` is only allowed in the `lazybox-tui` binary crate.
3. **Core 4 isolation.** `lazybox-core`, `lazybox-auth`, `lazybox-events`, `lazybox-store` must NOT depend on each other. Provider crates depend on core + events + auth only.
4. **Tests with every change.** Every public function has a test; every TUI component has a render snapshot (insta + ratatui `TestBackend`); every bug fix lands with a regression test. See `crates/tui/tests/` for the realm-level effect-contract tests.
5. **No real subprocesses in tests.** No real `claude` / `sh` / `curl` / `tmux` invocations. Mock at the trait boundary (`Agent`, `SessionBackend`, `CredentialProvider`) or `#[ignore]`-gate.
6. **Every async test needs a timeout.** Wrap the body in `tokio::time::timeout` — no exceptions. `cargo-nextest` enforces a 10s slow-test budget; relying on it is fine for healthy tests but explicit `timeout` is the rule for ones that historically hung.
7. **Don't add error handling / fallbacks for scenarios that can't happen.** Trust internal code + framework guarantees. Only validate at system boundaries (user input, external APIs).
8. **Don't add features behind feature flags.** Land code paths or don't land them. Half-implemented branches behind `cfg(feature = ...)` get reverted on review.

## Architecture cheatsheet

Lazybox is a Rust workspace organised as a client/daemon split with shared library crates:

```
crates/
  # ── shared libraries ────────────────────────────────────────────────
  core/           # Task, Workspace, Project, SessionKey. Source-agnostic.
  auth/           # CredentialProvider trait + chain.
  events/         # In-process event bus.
  store/          # Store trait + SQLite backend.
  config/         # YAML loader for ~/.lazybox/config.yaml.
  git-ops/        # Worktree manager.
  tui-term/       # Embedded terminal widget.
  tui-core/       # Action catalog + intent resolvers.

  # ── providers ───────────────────────────────────────────────────────
  gh-provider/    # GitHub PRs + Issues.
  linear-provider/ # Linear issues.

  # ── daemon-side ─────────────────────────────────────────────────────
  ipc/            # Command/Event wire types.
  agents/         # Agent trait + claude/codex/cursor/generic.
  llm-proxy/      # 127.0.0.1 HTTP pass-through.
  server/         # Server library.

  # ── client / binary ─────────────────────────────────────────────────
  tui/            # Component-tree TUI client + `lazybox` binary.
```

More detail in [`CLAUDE.md`](./CLAUDE.md) (architecture decisions, key patterns).

## Sample config

`~/.lazybox/config.yaml`:

```yaml
repos:
  - org: acme
    only:
      - widget
display:
  show_inactive_in_inbox: false
ui:
  short_snooze: 4h
  long_snooze: 365d
attention:
  unread: true
  ci_failure: true
  changes_requested: true
```

## Status / known gaps

- **Windows**: 6 `TODO(windows)` markers across `platform.rs` + `transport.rs`. The Unix socket transport, `setsid()`-based detach, and OS notification glue all need Windows ports.
- **Linear mutations**: `merge` for Linear (issue → state=done) not yet implemented. `add_assignees` works but needs display-name → UUID resolution.
- **Vendored libghostty**: `crates/libghostty-vt-sys/build.rs` fetches a pinned ghostty commit at build time. Zig 0.15.2 is pinned because newer Zig breaks the upstream `requireZig` check.

## Where to get help

- **Questions, setup help, sharing configs** → [GitHub Discussions](https://github.com/AntoineToussaint/lazybox/discussions).
- **Bugs and feature requests** → [Issues](https://github.com/AntoineToussaint/lazybox/issues/new/choose) (use the templates).
- **Docs & architecture** → the [docs site](https://docs.lazybox.ai/), plus [`CLAUDE.md`](./CLAUDE.md) and `DESIGN.md` for deeper notes.

lazybox is pre-1.0, so support is best-effort. See [`SUPPORT.md`](./SUPPORT.md) for the short version.

## Reporting bugs & requesting features

Use the [issue templates](https://github.com/AntoineToussaint/lazybox/issues/new/choose) — there's a form for bugs and one for feature requests. For bugs, include:

- The relevant `/tmp/lazybox.log` excerpt (re-run with `RUST_LOG=lazybox=debug` for verbose output).
- Your OS (macOS or Linux) and the commit you built from (`git rev-parse HEAD`).
- For terminal-rendering bugs, a screenshot.

For feature requests, lead with the problem you're hitting, not just the feature you have in mind.
