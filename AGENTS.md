# AGENTS.md

Guidance for any coding agent working in this repository (Claude Code, Codex,
Cursor, …). This file is the canonical source; `CLAUDE.md` points here.

## What is Lazybox V2?

A development environment across phone, iPad, laptop and several displays. A
**Runner** (Go, a Codefly `go-grpc` service) executes agents, commands,
terminals and workspaces; **clients** (a shared React/TypeScript surface hosted
by Tauri on the desktop and by a browser on the phone) show the same state. The
coordinating **Hub** is planned, not built. Start with [README.md](README.md);
it indexes every design document.

V1 (the Rust TUI and daemon) lives in
[lazybox-v1](https://github.com/AntoineToussaint/lazybox-v1). Nothing from V1
is imported without the review in [REUSE_AUDIT.md](REUSE_AUDIT.md). The one
admitted link is the snippet catalog helper in `tools/lazybox-snippets`, which
links V1's `lazybox-config` library at a pinned revision.

## How to behave

These rules outrank convenience, and they outrank finishing fast.

**A gap in the tooling is a bug in the tooling — never a reason to reach
around it.** If the `Makefile`, a script or a Runner API cannot express what
you need, the deliverable is the missing capability or a precise issue asking
for it. Not a workaround, not "just this once".

**Never hack. Always provide the best fix, even when it spans repos.** When
the right fix lives in another repository (Codefly, a provider SDK, V1), open
the PR there. If it cannot be fixed now, ship a precise issue against the
owner *plus* an explicitly-labelled stopgap.

**Classify every change that makes something work**, in the PR body: a *fix*
at the place that owns the behaviour, or a *hack*.

**Never hardcode what the system resolves.** State and environment roots come
from the launcher (`scripts/environments.mjs`, `LAZYBOX_V2_*` settings), ports
are allocated by Codefly or bound by the OS, credentials come from the
selected account profile, and the Runner endpoint comes from its owner-only
`endpoint.json`. A literal path or port works on one machine for ten minutes.

**Diagnose, do not pattern-match.** "It started working when I changed X" is
not a diagnosis: set X back and confirm it breaks.

**Say what you did not verify.** Unverified is not the same as working. If you
could not exercise a path — no provider login, no phone, a gate you skipped —
the PR body says so.

## Boundaries

- **Generated code is never hand-edited.** Codefly generates the Runner's
  composition and Go RPC code (`services/runner/code/main.go`, `pkg/adapters`,
  `pkg/gen`); Buf generates `packages/protocol/src/gen`. Change
  `services/runner/proto/api.proto` and run `make generate`; `make
  check-generated` fails on drift.
- **The wire contract is checked.** `buf breaking` runs against
  `contracts/runner-v1.binpb`. Updating the baseline is an explicit contract
  decision, recorded in [DECISIONS.md](DECISIONS.md).
- **Agents outlive the Runner.** Agents, their managed commands and terminals
  run in session hosts that survive a Runner restart
  ([SESSION_HOSTS.md](SESSION_HOSTS.md)). A change that ties their lifetime to
  the Runner process is a regression, whatever else it fixes.
- **Dev and Working are the user's live environments.** Never stop, restart,
  stage over or promote them unless asked. Their state lives under
  `.cache/environments` (or `LAZYBOX_V2_ENVIRONMENTS`) and holds live agents.
- **Tracker records need the user's go-ahead.** Do not file an issue, PR or
  ticket unless the user asked for it.

## Build, run, test

```bash
make dev               # build and run isolated Lazybox Dev (macOS)
make check             # format, vet, typecheck, generated code, Go and client tests
make check-desktop     # Tauri crate: fmt, tests, clippy
make check-release     # the full deterministic release catalog
make test-outlive      # race-checked restart-survival tests
node scripts/release-gates.mjs run ui --only <ids>   # selected UI gates
```

`make help` lists everything. [RELEASE_GATES.md](RELEASE_GATES.md) is the gate
catalog CI runs; [RUNNING.md](RUNNING.md) covers launching and options;
[ENVIRONMENTS.md](ENVIRONMENTS.md) covers Dev, Working, staging and promotion.

**This box is shared with other agents.** The Makefile defaults to one Go
build job, `GOMAXPROCS=1` and one Cargo job. Sample the load (`uptime` against
the core count) before raising them. Throttling changes how hard you build,
never whether the full gate runs before you push.

## Conventions

- Conventional Commits for commit messages and PR titles.
- Every behaviour change lands with a test; every bug fix with a regression
  test that fails without the fix. UI behaviour is covered by a Playwright
  suite registered in `scripts/release-gates.json` — an unclassified `test:`
  script fails the catalog.
- Docs are code: the PR that changes behaviour updates the document that
  describes it.

## Where things live

```
services/runner/        the Runner: proto/api.proto and the Go module in code/
packages/               protocol (generated), client-core, platform, surfaces
apps/desktop/           Tauri host (its own Cargo workspace and lockfile)
apps/web/               browser host and the phone app
workers/claude-sdk/     Claude Agent SDK worker
scripts/                launchers, release gates, UI suites
tools/lazybox-snippets/ snippet catalog helper (links V1's lazybox-config)
contracts/              Buf compatibility baseline
audits/                 dated audits; historical, not current guidance
```

[STRUCTURE.md](STRUCTURE.md) describes package boundaries in detail.
