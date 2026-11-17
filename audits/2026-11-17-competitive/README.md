# Are we beating Conductor, herdr and every other agent GUI? — 2026-11-17

The final competitive audit before the Lazybox V2 0.1 prerelease. It
compares the product on `feat/v2-command-output` (at `21597f37e`, plus this
branch) with the agent GUIs as they stand this week. It builds on the
[2026-11-16 audit](../2026-11-16/README.md), the
[herdr GUI re-audit](../2026-11-16/herdr-gui.md),
[sota-agents.md](../2026-11-16/sota-agents.md) and the Mobile/Cloud/Hub proposal
(`MOBILE_CLOUD_HUB.md`, on `docs/v2-mobile-cloud-hub`, not yet merged here).

**Method.**

- **Ours, exercised.** I ran 58 of the 112 UI fixture suites
  (`node scripts/test-*.mjs`, against the fixture Runner
  `scripts/fixtures/runner-ui.mjs`), choosing the ones that back a scorecard
  claim. I looked at the screenshots they write to `.cache/`. I also ran the
  Go packages I touched (`internal/runner/...`, `internal/transport`,
  `internal/gitpublish`), all green.
  - **Result: 56 pass, 2 fail reproducibly.**
    - `test-agent-review.mjs:179`: a Codex review card stays on "reviewing turn 1"
      and never reaches "2 major".
    - `test-command-palette.mjs:68`: two `[data-agent-composer]` textareas are on
      the page, both `disabled`.
    - `agents-visible` and `resizable` failed once in the batch run and passed when
      rerun alone. The first was load-related; the second was invoked without
      `--import tsx`, which `package.json` adds.
  - **Fixture runs are not real-agent evidence.** Unless a row says *live*, the
    evidence is the fake Runner.
- **Theirs, read.** I relied on vendor docs, changelogs, pricing pages and repos,
  read on 2026-11-17 and linked in [Sources](#sources). I did not run any
  competitor. Rows that rest on secondary sources are marked **[UV]**.

## 1. The verdict in one paragraph

**No — not yet, and against most of them not on the axis that decides
adoption.** On a fixture, Lazybox V2's desktop is the most *complete
local-first workbench for supervising several agents* of any product here:
- one chat for Claude, Codex and nine ACP agents;
- cross-agent review;
- a PR inbox that tracks what agents push;
- per-turn checkpoints;
- an orchestrator with plan approval, a dollar cap and leases;
- agents that survive Runner restarts and app updates.

Against **herdr** we clearly win on *understanding and verifying* the work: herdr
renders terminals and has no review, diff or PR. Against **Conductor**, our
closest analogue, we are at par or ahead on the desktop. Conductor has been
unsettled by its Meta acquisition and reversal, which is an opening.

But both of them **ship**. herdr has 51k stars, Mac/Linux/Windows, and free
native iPhone, iPad and Android apps. Conductor has had iOS remote control since
October, plus cloud workspaces. Lazybox has:
- no public release, no public repository and no license;
- Apple Silicon Macs only;
- a phone client that is a tailnet PWA never tested on a real iPhone;
- no execution anywhere but an awake laptop;
- real-agent evidence that covers basic Claude/Codex turns and the
  outlive-the-Runner path, and nothing of orchestration, review, the PR loop or
  the phone.

Against the first-party apps, the gap is in reach, not in the desktop:
**Claude Code** (desktop + web + Remote Control + Projects), **Codex** (ChatGPT
app + cloud + mobile), **Cursor 3** (Agents Window + cloud VMs + iOS) and the
**Copilot app** (Agent HQ, cloud sandboxes, phone-steered local sessions) are all
behind us in provider neutrality and local-first trust, and ahead in everything
that gets a product used: distribution, cloud, phone, Windows and Linux, and
live-tested scale.

We are **winning on depth of the local supervision loop** and **losing on reach
and proof**. The prerelease fixes neither by itself.

## 2. Scorecard

Verdicts: **lead / par / behind / missing**. "Best" names the best-in-class
product today. Evidence for us is a file or suite in `v2/` (✓ = ran green today;
✗ = ran red today; *live* = exercised with a real agent). Evidence for them is a
URL from [Sources](#sources).

### Start work

| # | Capability | Best in class | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 1 | Prompt → workspace + branch + agent in one step | Claude Code desktop, Conductor | **par** | `test-new-task` ✓ | [C1], [A3] |
| 2 | Start from a GitHub issue (paste or pick), issue text quoted as data | Copilot (assign issue), Conductor | **par**; the quoting as untrusted data is better than most | `test-start-from-issue` ✓, screenshot `start-from-issue-sheet-light.png` | [G1], [C1] |
| 3 | Start from Linear / Slack / Jira | Cursor, Devin, Codex | **missing** (V1 has providers; V2 has GitHub only) | `docs/guides/start-from-an-issue.md` | [CU1], [D1], [X3] |
| 4 | Several repositories in one task | Lazybox locally; Cursor cloud multi-repo | **lead** (local) | `test-unified-repositories` ✓, `test-workspace` ✓ | [CU1] |
| 5 | Scheduled / triggered agents | Claude routines, Codex scheduled tasks, Cursor automations, Devin | **missing** (hooks start programs on events; nothing starts an agent on a schedule) | `POWER.md` | [A2], [X3], [CU2] |
| 6 | A new branch starts from origin's current default branch | Codex, Conductor (table stakes) | **par** — fixed on this branch; before, a source copy branched from its clone-time `HEAD` | `internal/runner/fresh_base_test.go` ✓ | — |

### Parallelism and workspaces

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 7 | Worktree per task | everyone | **par** | `WORKSPACES.md` | [C1], [X2], [Z1] |
| 8 | Container / VM isolation per task | Sculptor, every cloud agent | **missing** | `RESOURCE_OWNERSHIP.md` (no containers) | [S1], [CU1] |
| 9 | Agents outlive app close, Runner restart and update | Lazybox, herdr | **lead** (shared with herdr), *live* for Claude once | `SESSION_HOSTS.md`, `make test-real-outlive` (RUNTIME_EVIDENCE), `test-runner-restart` ✓, `test-reattach` ✓, `test-updates` ✓ | [H1] FAQ |
| 10 | Run / preview the app per workspace, port block | Conductor, Claude desktop | **par** | `test-workspace-runtime` ✓ | [C1], [A3] |
| 11 | Several machines in one window | herdr (SSH), Claude Remote Control server, Codex hosts | **behind**: local Runners only; remote is step 3 and not built | `test-multi-runner` ✓, `MULTI_RUNNER.md` | [H1], [A1], [X4] |
| 12 | Platforms | Copilot app, Claude, Codex, herdr, Zed: Mac, Windows and Linux | **behind**: Apple Silicon Mac only | `docs/install.md` | [G2], [H2], [Z1] |

### Chat fidelity

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 13 | Structured chat: tool lines, plans, sub-agents, live bar | Claude Code desktop, Codex app | **par** | `test-rich-activity` ✓, `test-subagents` ✓, `test-live-bar` (not run) | [A3], [X2] |
| 14 | Many agents in one chat UI (Claude, Codex, ACP) | Zed, Devin Desktop, Warp, Happy | **par**; Claude and Codex are native, nine ACP agents listed; one live Gemini turn is documented but was not run today | `test-acp` ✓, `ACP.md` | [Z2], [D2], [W1] |
| 15 | Attachments: images, PDFs, large pastes as boxes | Claude desktop | **par** | `test-attachments` ✓ | [A3] |
| 16 | Long conversations open fast | Zed (native) | **par** (fixture: about 10k events, last turn shown in 1/5 of the old time) | `test-transcript-performance` ✓, `test-conversation-load` ✓ | [Z1] |
| 17 | Context and cost per part (input, cache, output) | Lazybox | **lead** | `test-telemetry` ✓, CHANGELOG `.3`/`.26` | [A3] |
| 18 | Conversations as tabs, split, separate windows | Cursor Agents Window, Claude desktop | **par** | `test-conversation-tabs` ✓ | [CU2], [A3] |

### Control

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 19 | Permission modes, changed while running | Claude (Auto classifier), Codex ("Approve for me") | **behind**: Ask/YOLO-style modes with live change, but no automatic-reviewer mode | `test-settings-approval` ✓ | [A3], [X5] |
| 20 | Queue / steer / Send now | Zed, Codex, Lazybox | **par** (⌘↵ Send now, steer for Codex) | `test-queued-messages` ✓, `test-send-recovery` ✓ | [Z2] |
| 21 | Approvals with risk flags; answer questions with chips | Claude Remote Control | **par** | `test-chat-decisions` ✓, `remote-inbox-dark.png` | [A1] |
| 22 | Several subscriptions, automatic switching at limits | Lazybox (alone) | **lead, and a liability** (see §4, item 5) | `test-subscription-switch` ✓, `SUBSCRIPTIONS.md` | [A5] |

### Review and diff

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 23 | Diff review, inline comments sent back to the agent | Codex app, Claude desktop, Conductor | **par** | `test-review-comments` ✓ | [X2], [A3], [C2] |
| 24 | Split diff, hunk navigation, viewed files | Codex, Zed | **par** | `test-split-diff` ✓ | [X2], [Z2] |
| 25 | Per-turn checkpoints, revert with preview | Conductor, Cursor | **par** | `test-turn-checkpoints` ✓ | [C1] |
| 26 | Another agent reviews (Codex reviews Claude), review loop | Copilot `/rubberduck`, Devin Review | **lead** locally, but **one suite is red** | `test-review-from-chat` ✓, `test-review-loop` ✓, `test-agent-review` ✗ | [G1], [D1] |
| 27 | Edit in the diff / editor | Conductor | **par** | `test-editor-navigation` (not run) | [C1] |

### PR and issue loop

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 28 | Commit, push, open a PR from the app | everyone | **par** | `test-pull-request` ✓ | [C2], [X2] |
| 29 | Track PRs the agents opened themselves, checks, review state, inbox | Conductor Checks tab, Codex PR inbox | **par** | `test-pull-requests` ✓, `pull-requests-panel.png` | [C2], [X6] |
| 30 | CI failures and review comments fixed without a click | Claude auto-fix, Copilot Agent Merge, Cursor Bugbot autofix | **behind** by design (one click, never automatic) | `GITHUB.md` | [A2], [G1], [CU3] |
| 31 | Merge when green / auto-merge | Claude "merge when ready", Copilot | **behind** (merge with confirmation; no auto-merge) | `test-pull-requests` ✓ | [A2], [G1] |
| 32 | Post a review or replies to GitHub | Claude auto-fix, Codex `@codex review` | **missing** | 2026-11-16 audit, "still open" | [A2], [X6] |

### Orchestration and missions

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 33 | An agent runs agents after the person approves its plan | Warp `/orchestrate`, Conductor 0.95, Claude teams/Projects | **lead** on fixture (plan sheet, concurrency, permission never above its own) — **never run with real agents** | `test-orchestration` ✓, `test-orchestration-plans` ✓, `ORCHESTRATION.md` "Not verified with real Claude or Codex" | [W2], [C3], [A4] |
| 34 | Dollar budget enforced per agent tree | nobody else | **lead** (fixture) | `test-orchestration-plans` ✓ | [sota §3g](../2026-11-16/sota-agents.md) |
| 35 | Causal timeline, agent-to-agent messages, leases, overlap alerts | nobody else complete | **lead** (fixture) | `test-orchestration-board` ✓, `orchestration-board-dark.png` | [sota §3f](../2026-11-16/sota-agents.md) |
| 36 | Mission board with evidence states ("No evidence", "Observed") and acceptance | Claude Projects (partly) | **lead** (fixture) | `test-missions` ✓ | [A4] |
| 37 | Proven at scale (tens of agents) | Copilot `/fleet` (32), Claude Remote Control server (32), Devin | **behind**: default 3, maximum 16, never measured live | `ORCHESTRATION.md` | [A1], [sota] |

### Artifacts and docs

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 38 | Versioned plans and reports, compare versions, find from anywhere | Antigravity artifacts | **par** | `test-artifacts` ✓, `test-find-artifacts` ✓, `test-artifact-reader` ✓ | [sota] |
| 39 | Agent browser the person watches and can take over | Claude desktop browser pane, Cursor | **par** | `test-agent-browser` ✓ | [A3], [CU2] |
| 40 | Design mode: point at UI, annotate, send to the agent | Cursor Design Mode, Devin | **missing** | (not found in `v2/`) | [CU2], [D1] |
| 41 | Offline docs inside the app | Lazybox | **par** (small) | `test-help` (not run), CHANGELOG 0.1.0 | — |

### Phone and remote

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 42 | Native phone app | Cursor iOS, Conductor iOS, herdr (iOS/Android), Claude, ChatGPT/Codex, Kilo | **missing**: PWA only; `apps/iphone` is a placeholder | `REMOTE.md` | [CU4], [C4], [H3], [A1], [X4] |
| 43 | Inbox: approve and answer from the phone | Claude Remote Control | **par on paper**: tailnet only, fixture only, **never on a real iPhone** | `test-remote-inbox` ✓, `test-remote-pairing` ✓, `REMOTE.md` "Not verified" | [A1] |
| 44 | Push with actions, Live Activities | Cursor (Live Activities), Kilo (approve buttons), GitHub Mobile | **behind**: Web Push, no actions, unverified on iOS | `test-remote-push` ✓ (mocked `PushManager`) | [CU4], [K1], [G3] |
| 45 | Reach without a VPN | Claude Remote Control, Codex, Conductor | **missing** | `REMOTE.md` | [A1], [X4], [C4] |
| 46 | End-to-end encrypted relay | Happy | **par** for now (WireGuard on the tailnet); the advantage is gone the day we add a relay without E2E | `MOBILE_CLOUD_HUB.md` §3 | [HP1] |
| 47 | Review diffs, start work, merge from the phone | Cursor, Conductor (cloud) | **missing** (screens not built) | `MOBILE_CLOUD_HUB.md` §1.2 | [CU4], [C4] |

### Cloud and execution away from the laptop

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 48 | Work continues with the laptop closed | Claude web, Codex cloud, Cursor, Devin, Copilot, Amp | **missing** | `MOBILE_CLOUD_HUB.md` §1.3 | [A2], [X3], [CU1], [D1] |
| 49 | Self-hosted always-on runner, hosted control plane | Claude self-hosted envs, Cursor self-hosted machines, Copilot `--environment-host`, Amp self-hosted orbs | **missing** (the Runner does not build for Linux) | `IMPLEMENTATION_ROADMAP.md` P02 | [A2], [CU2], [G4], [AM1] |

### Extensibility

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 50 | Plugins / MCP / hooks / marketplace | Claude plugins, Cursor marketplace | **behind**: hooks, actions and CLI parity, but no plugin format or marketplace; MCP is whatever each agent loads | `POWER.md`, `docs/guides/automate.md` | [A3], [CU2] |
| 51 | Panels you add | herdr (576 mods, unsandboxed) | **par**: safer, sandboxed and granted, but no ecosystem | `test-extensions` ✓, `EXTENSIONS.md` | [H1] |
| 52 | A control plane agents can drive (spawn, send, wait) | herdr `hc`, Conductor MCP/API | **par** | `lazybox agents …`, `docs/reference/cli.md` | [H1], [C3] |

### Keyboard and layout

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 53 | Docking, multiple windows, keyboard panel navigation, put-back | Lazybox among agent GUIs; Zed in editors | **lead** against Conductor, Codex and Claude desktop | `test-docking` ✓, `test-layout-keyboard` ✓, `test-resizable` ✓ (rerun) | [Z1] |
| 54 | Command palette over every action | Zed, Codex ⌘K, herdr | **par**, with a red suite | `test-command-palette` ✗ | [X2], [H1] |
| 55 | Voice: on-device dictation, spoken commands, read-aloud | Cursor (realtime voice calls), Happy | **par** (no conversational voice) | `test-voice` ✓, `VOICE.md` | [CU2], [HP1] |

### Polish and performance

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 56 | Native rendering speed | Zed / herdr (GPUI) | **behind**: Tauri webview; Conductor just moved to Electron | — | [Z1], [H2], [C3] |
| 57 | Visual quality | Conductor, Claude desktop | **par** from screenshots (dense, consistent, dark/light); no outside user has seen it | `.cache/agents-visible-*.png`, `remote-*.png` | — |

### Onboarding and install

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 58 | Install from a public, signed build or brew | herdr (`brew --cask`), everyone | **missing**: "No release is published yet"; unsigned until an Apple Developer ID exists | `docs/getting-started.md`, `RELEASING.md` | [H2] |
| 59 | First-run welcome, doctor, updates that keep agents running | Lazybox | **lead** (updates keep agents running) | `test-onboarding` ✓, `test-updates` ✓ | — |

### Pricing and business model

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 60 | Free local use with your own subscriptions | herdr, Nimbalyst, Happy, Conductor local | **par** in intent | — | [H1], [N1], [HP1] |
| 61 | Open source | herdr (Apache-2.0), OpenCode (MIT), Happy (MIT) | **behind**: the root `LICENSE` is "All rights reserved"; the export adds none | `/LICENSE`, `scripts/export-repo/root/` | [H2], [O1] |
| 62 | A paid tier that pays for something | Paradev Pro $20 (mobile + remote), Cursor Premium $30 (cloud) | **missing** | — | [P1], [CU5] |

### Trust and security

| # | Capability | Best | Lazybox | Our evidence | Their evidence |
|---|---|---|---|---|---|
| 63 | Local-first, no telemetry, feedback read before it is sent | Lazybox, herdr | **lead** | `test-feedback` ✓ | [H1] |
| 64 | Agents cannot approve themselves; scoped phone grants; revoke | Claude Trusted Devices | **lead** (credentials per execution, allowlisted device RPCs) | `RESOURCE_OWNERSHIP.md`, `TestAuditAgentCredentialCannotApproveOrAnswer` | [A1] |
| 65 | Compliance (SOC 2, ZDR, enterprise) | Cursor, Devin, Warp | **missing** (fine for a local tool, required for the Hub) | — | [CU6], [D3], [W3] |
| 66 | Clean use of provider subscriptions | Codex "Sign in with ChatGPT" partners | **behind / at risk** (§4, item 5) | `workers/claude-sdk/package.json` | [A5], [X7] |

**Tally (66 rows, by each row's headline verdict):** lead 13 · par 29 ·
behind 11 · missing 13. Seven of the 13 leads are rows where nobody else
competes (17, 22, 34, 35, 36, 59, 63). On contested rows we lead on 6 and are
behind or missing on 24.

## 3. Where we clearly lead — the moat, and whether it holds

1. **Supervising several agents with evidence**: orchestrator grants with plan
   approval, a dollar cap per agent tree, leases and overlap alerts, a causal
   timeline, and a mission board where "done" is a claim until evidence is
   observed (rows 33–36).
   - **Is it defensible? Partly.** It is the one area where the survey found no
     equal. Warp has the plan gate. Claude Projects and Cursor Projects have
     coordinators. Nobody enforces budgets or shows causality.
   - But it is **fixture-only**: "Not verified with real Claude or Codex agents"
     (`ORCHESTRATION.md`). Conductor shipped agents-that-manage-workspaces on
     Nov 16 [C3]. The idea is copyable in a quarter. What is hard to copy is the
     *local, open event log* (`internal/work`), which outlives any vendor. That
     matters in a market where Conductor, Vibe Kanban and Terragon each nearly
     or actually disappeared this year.
2. **Provider-neutral, owned conversation** (rows 14, 26): Claude and Codex
   natively plus ACP, with Codex reviewing Claude.
   - **Weak moat.** Zed, Devin Desktop, Warp, Nimbalyst, Happy and OpenCode all
     run several agents. Only the first-party apps cannot, and they will not
     start.
   - It holds as long as we are the best *local* multi-provider review loop.
     Today one of its suites is red.
3. **Agents outlive everything but a reboot** (row 9).
   - **Not a moat; table stakes we got early.** herdr does the same, and Codex
     auto-resumes after a crash [X8].
4. **Trust posture** (rows 63, 64): no telemetry, scoped credentials, an agent
   cannot approve itself, phone grants.
   - **Defensible if the relay and Hub stay end-to-end encrypted.** Anthropic's
     Remote Control stores transcripts and is unavailable under ZDR/HIPAA [A1].
     Only Happy matches us [HP1].
   - This is the strongest business wedge, not a feature lead.
5. **Desktop layout and keyboard** (row 53): docking, windows and put-back
   across agent GUIs.
   - **Not a moat.** It is polish that a funded competitor matches in one
     release.

**Bottom line:** the moat is *evidence-based supervision of many agents, on
data the person owns, reachable privately from anywhere*. Today only the first
half of that sentence exists, and it is unproven with real agents.

## 4. Where we are behind, ranked by how many users it costs

1. **Nothing is installable.**
   - No release, no public repository, no license, and unsigned until an Apple
     Developer ID exists (rows 58, 61).
   - Every other product in this audit can be installed today.
   - Cost: **all users**.
2. **Mac on Apple Silicon only** (row 12). Claude, Codex, Copilot, herdr, Zed
   and Warp run on Windows and Linux. Cost: most of the market.
3. **No phone app; the PWA has never touched a real iPhone** (rows 42–47).
   - `REMOTE.md` lists "a real phone on a tailnet" as not verified.
   - Cursor, Conductor, herdr, Claude, Codex, Kilo and Happy ship native apps.
     Several have push with actions or Live Activities.
   - This was the product's stated dogfood goal on Nov 9 (`PRODUCT.md`).
4. **Execution stops when the laptop sleeps** (rows 48–49). Every first-party
   product and Devin, Amp, Cursor and Copilot run in the cloud or on
   self-hosted runners. We have no Linux Runner build.
5. **Provider policy risk.** Two problems:
   - **The Claude worker uses the Agent SDK** (`@anthropic-ai/claude-agent-sdk`).
     Anthropic's docs say third-party products may not offer claude.ai login or
     rate limits "including agents built on the Claude Agent SDK" unless
     approved. The stated exception is the person signing in to the
     **unmodified Claude Code binary** [A5].
     - Anthropic enforced this against third-party harnesses in 2026
       (secondary sources [A6]).
     - Conductor ships the same pattern, the SDK with the person's Claude plan
       [C5]. Whether it has Anthropic's approval is not public.
   - **Automatic rotation between personal subscriptions when one hits its
     limit** (row 22) plausibly reads as "bypassing protective measures" under
     the Consumer Terms [A7]. That is my inference.
   - OpenAI, by contrast, supports Sign in with ChatGPT for third-party apps;
     open-source and local apps may use app-server [X7].
   - Cost: potentially **every Claude user**, overnight.
6. **Real-agent evidence is thin.** `RUNTIME_EVIDENCE.md`:
   - **Live Claude:** a basic conversation, a file upload, and outliving the
     Runner once.
   - **Live Codex:** multi-turn, questions and approvals.
   - **Never live:** the full Claude image/tool/interrupt suite rerun, Codex
     steering, two-account rotation, orchestration, the review loop, the PR loop,
     ACP beyond one documented Gemini turn, and the phone.
   - Cost: trust, and the bugs our first users will find instead of us.
7. **No Linear, Slack or Jira intake, no schedules** (rows 3, 5). Teams start
   work from trackers and chat; Cursor, Devin and Codex accept @-mentions there.
8. **The PR loop needs a human at every step** (rows 30–32). Claude auto-fix,
   Copilot Agent Merge and Bugbot close the loop on their own. "Never without
   your click" is a fine default, but there is no opt-in autopilot.
9. **Community and mindshare.** herdr has 51k stars, OpenCode 227k, Happy 25k.
   The audience is already in someone else's app.
10. **Two red UI suites today** (rows 26, 54). They are small, but this is the
    review feature we call our lead, on the eve of a prerelease.

Not counted as losses, by decision:
- **No terminal emulator** (herdr's core). See the
  [decision](../2026-11-16/herdr-gui.md#decision-no-terminal-emulator).
- **No IDE.**

## 5. Kill list — the 10 things that would most change the verdict

| # | Change | Rows it moves | Effort | Agent can build now? |
|---|---|---|---|---|
| 1 | **Publish 0.1.0-pre.1 properly**: a signed and notarized DMG, the brew cask, a public repository with a real license (decide: Apache-2.0 / MIT for the local product, or FSL/BSL), and the docs site. | 58, 61, 60 | S–M | Mostly. Needs the user for the Apple Developer ID, the license and the go-public call. |
| 2 | **Claude compliance**: drive the unmodified `claude` binary (or get written approval for the SDK path); make subscription rotation opt-in, labelled, and off for Claude by default; offer API-key mode. | 22, 66 | S–M | Yes for the code; the policy decision is the user's. |
| 3 | **A recorded live acceptance run**: Claude + Codex + one ACP agent through issue → work → cross-review → PR → CI fix → merge; an orchestrator with two children under a cap; a real iPhone approving on the tailnet. Publish it in RUNTIME_EVIDENCE. | 9, 26, 33–37, 43 | M | Harness yes; needs live subscriptions and a phone. |
| 4 | **Linux Runner + BYO always-on Runner** (a VM or a Mac mini), paired from the desktop (MULTI_RUNNER step 3). | 11, 48, 49 | M | Yes. |
| 5 | **Native iPhone app (Expo) on TestFlight**: Inbox, approve from the notification, Live Activity per running turn, diff review, start work. | 42, 44, 47 | L | Code yes; Apple account and TestFlight need the user. |
| 6 | **E2E relay + push gateway** (V1's Noise design over WSS), opt-in per Runner. | 45, 46, 44 | M–L | Yes against local fakes (M1); hosting needs the user. |
| 7 | **PR autopilot as an opt-in grant**: watch the PR, send CI failures and review comments automatically within a budget, merge-when-green, post findings to GitHub. | 30–32 | M | Yes. |
| 8 | **Linear and Slack intake** (port V1 `linear-provider` and `slack-provider`), with scheduled runs. | 3, 5 | M | Yes. |
| 9 | **Fix the two red suites and gate merges on the full UI suite set.** | 26, 54, 57 | S | Yes. |
| 10 | **Windows and Linux desktop builds** (Tauri supports both; the Runner's process groups and session hosts are POSIX). | 12 | L | Yes, with real machines to verify. |

Items 1–3 change "we have shipped nothing and proved little" into "a small,
honest, installable product". Items 4–6 change the phone and cloud rows from
missing to par. Item 6 is the only one that can turn into a **lead**: an
end-to-end-encrypted relay with push actions, which no first-party vendor offers.

## 6. The TODO that stays in view: iPhone app, Hub, remote access

### How each changes the scorecard

| Deliverable | Rows | Before → after |
|---|---|---|
| **Remote access without the Hub**: an E2E relay the Runner dials out to, pairing QR v2, push gateway; the tailnet stays the default | 45, 46, 44 | missing → par on reach; par → **lead** on relay privacy (only Happy matches); push with actions still needs the native app |
| **iPhone app** (Expo, keeping the PWA) | 42, 43, 44, 47, 55 | missing → par for the app; behind → par/lead for push (approve from the Lock Screen, a Live Activity per turn); diff review and start work on the phone → par |
| **Hub MVP**: accounts with passkeys, Runner directory, device introductions, cross-Runner inbox with titles end-to-end encrypted | 11, 49, 62, 65 | several machines behind → par; it creates the paid tier (row 62); compliance becomes a real requirement |
| **Hub + BYO / managed Runners** | 48, 49 | missing → par for laptop closed. A managed Runner is the only way to reach par with Claude web, Codex cloud and Cursor cloud without asking people to run a VM. |

Together the three move 10 rows. At best the tally becomes lead 15 · par 35 ·
behind 9 · missing 7. We would still be behind on platforms, install and live
proof, which none of the three touch; that is why kill-list items 1–3 come
first.

### Order to build them

1. **This week, no new infrastructure.**
   - Put the existing PWA on a real iPhone over Tailscale: pair, push,
     approve, kill the app, reboot the phone. Fix what breaks.
   - Build the missing phone screens: diff review, start a conversation, PR
     list. This is MOBILE_CLOUD_HUB M0; an agent can do all of it.
   - Without this, every later phase builds on an unverified client.
2. **In parallel, the groundwork (M1).**
   - The Linux Runner build.
   - A `KeyProvider` abstraction.
   - The relay protocol and its Go client, against local fakes.
   - This unblocks both the BYO always-on Runner and the relay. An agent can do
     all of it.
3. **The iPhone app on TestFlight over the tailnet (M2).**
   - Start the Apple Developer enrollment now, because Beta App Review is the
     slow part.
   - The native app is what gives push actions and Live Activities. Those are
     the phone features worth more than a PWA.
4. **Remote without the Hub (M3): the hosted E2E relay and push gateway.**
   - Free and rate-limited during the preview.
   - Ship this before the Hub, because reach is what the phone rows need.
     Anthropic, OpenAI and Cursor have made a plain relay free, so it can't be
     what people pay for.
5. **The Hub MVP (M4)**: identity, Runner directory, cross-Runner inbox,
   device introductions. **This is where payment starts.**
6. **Teams and hosted Runners (M5)**: shared projects, missions across Runners,
   audit, BYO-cloud enrollment, then managed Runners once the provider-terms
   question (§4, item 5) is settled for VMs we operate.

Remote access *with* the Hub is the paid product. Remote access *without* the
Hub (tailnet, or a self-hosted relay) stays free forever. That is the honest
offer against Claude Remote Control, which is free but readable by its vendor.

### Business model sketch

**Free, local and open** (the license is the decision in kill-list item 1):

- the desktop app, the Runner and the CLI;
- every agent and provider;
- workspaces, review, the PR loop, missions and orchestration;
- artifacts, voice and extensions;
- the PWA and the native app on your tailnet;
- a self-hosted relay.

**Paid ("Lazybox Hub"):**

| Tier | Price (proposal) | What it pays for |
|---|---|---|
| Personal | **$12–15 / month** | Hosted E2E relay without rate limits; push with actions and Live Activities; cross-Runner inbox; up to 3 Runners; one BYO-cloud Runner enrolled and supervised |
| Team | **$30–40 / user / month** | Shared projects; missions and PR tracking across people and Runners; audit log; org policy for which Runners admit whom; SSO later |
| Managed Runners | **compute at cost + 10–20%**, or GB-hour bundles | A Runner we host, for laptop-closed work; only after the terms check |

Comparisons (official pricing pages unless marked):

| Product | Price | What the money buys |
|---|---|---|
| Paradev (formerly Superset) | Free local; **Pro $20/mo** (mobile, remote access, automations, Linear/Slack); Team $60/user (cloud workspaces) [P1] | The closest analogue: mobile and remote behind the paywall |
| Conductor | Local free; Pro **~$50/mo**, Teams $60/user **[UV]**, official page shows no prices [C6] | Cloud workspaces, mobile |
| Cursor | Free; Plus $8; **Premium $30** (needed for cloud agents and Bugbot); Super $100; Ultra $200; Teams from $30/user [CU5] | Models and cloud VMs |
| GitHub Copilot | Free; Pro $10; **Pro+ $39**; Max $100; Business $19/user, Enterprise $39/user **[UV for Business/Ent.]**; AI credits since Jun 1 [G5] | Models, cloud sandboxes (billed from Nov) |
| Devin | Free; **Pro $20**; Max $200; Teams $80/mo minimum + $40/seat; Enterprise ACUs [D4] | Cloud VMs and Devin's agent |
| Warp | Free; **Build $20**; Max $200; Business $50/user [W3] | Model usage, Oz cloud agents |
| Happy | Free app and relay; **Plus $19.99/mo** for voice only [HP2] | Voice |
| Nimbalyst | Free incl. mobile; **Teams $20/user** (beta, free now) [N1] | Collaboration |
| Claude Remote Control | Included in Pro $20 / Max $100 / Ultra $200 [A8] | — (the relay is free) |

**Lessons from the market:**

- **Vibe Kanban.** It had 28k stars and free remote access, and its company
  still shut down because it "couldn't find a business model" [VK1].
- **Omnara.** It left phone control after Anthropic shipped Remote Control
  [OM1].
- **What sells.** People pay for *reach, persistence and team state*, not for a
  pipe:
  - always-on execution;
  - several machines;
  - shared missions;
  - privacy a vendor relay cannot offer.
- **Price.** Priced below the $20 anchors (Paradev, Devin Pro, Warp Build), a
  Personal tier is an easy add-on to the $20–200 people already pay Anthropic
  and OpenAI.

## Also on this branch

A separate commit (`fix(v2): a new workspace branch starts from origin's
default branch, fetched first`) closes a defect found while auditing row 6.

- **The defect.** Every worktree was created from the repository's `HEAD`, and
  nothing in the Runner ever fetched:
  - a GitHub source copy branched from its default branch *as of the clone*;
  - a registered checkout branched from whatever branch the person had out.
- **The fix.**
  - The Bench fetches origin's default branch (named by origin's HEAD) before
    planning members.
  - `collections` pins the new branch to `refs/remotes/origin/<default>`.
  - Offline, it falls back to the last known default branch, and says so in
    the preparation progress.
- **The tests:** `internal/runner/fresh_base_test.go`.

## Sources

Read 2026-11-17. **[UV]** = secondary source or not confirmed on the vendor's
own page.

**Conductor**
- [C1] https://www.conductor.build/docs
- [C2] https://www.conductor.build/docs/reference/checks
- [C3] https://www.conductor.build/changelog/0.95.0-agents-that-manage-your-local-workspaces
- [C4] https://www.conductor.build/changelog/0.92.0-remote-control and https://www.conductor.build/changelog/0.90.0-conductor-for-ios
- [C5] https://www.conductor.build/blog/claude-subscription-update
- [C6] https://aisotools.com/pricing/conductor **[UV]**
- Meta acquisition and reversal: https://www.conductor.build/changelog/0.93.0-conductor-is-joining-meta, https://www.conductor.build/changelog/0.94.2-conductor-is-back, https://siliconangle.com/2026/11/04/meta-acquires-ai-coding-startup-conductor-whose-app-runs-several-coding-agents-at-once/

**herdr**
- [H1] https://herdr.dev/ and https://herdr.dev/faq
- [H2] https://github.com/herdrdev/herdr-app and https://github.com/herdrdev/herdr
- [H3] https://herdr.dev/mobile

**Claude Code**
- [A1] https://code.claude.com/docs/en/remote-control
- [A2] https://code.claude.com/docs/en/claude-code-on-the-web.md
- [A3] https://code.claude.com/docs/en/desktop.md
- [A4] https://code.claude.com/docs/en/claude-projects.md and https://code.claude.com/docs/en/agents.md
- [A5] https://code.claude.com/docs/en/agent-sdk/overview and https://code.claude.com/docs/en/legal-and-compliance.md
- [A6] https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access, https://venturebeat.com/ai/anthropic-cracks-down-on-unauthorized-claude-usage-by-third-party-harnesses/ **[UV]**
- [A7] https://www.anthropic.com/legal/consumer-terms
- [A8] https://claude.com/pricing

**Codex**
- [X2] https://developers.openai.com/codex/app/review
- [X3] https://learn.chatgpt.com/docs/cloud.md
- [X4] https://learn.chatgpt.com/docs/remote-connections.md and https://www.macrumors.com/2026/05/15/openai-brings-codex-chatgpt-mobile-app/
- [X5] https://learn.chatgpt.com/docs/permission-modes.md
- [X6] https://learn.chatgpt.com/docs/code-review.md
- [X7] https://learn.chatgpt.com/docs/sign-in-with-chatgpt.md and https://developers.openai.com/siwc/token-sharing-open-source/codex-app-server
- [X8] https://learn.chatgpt.com/docs/changelog?type=codex-app

**Cursor**
- [CU1] https://cursor.com/docs/cloud-agent
- [CU2] https://cursor.com/changelog/3-0 and https://cursor.com/changelog
- [CU3] https://cursor.com/docs/bugbot
- [CU4] https://cursor.com/blog/ios-mobile-app
- [CU5] https://cursor.com/pricing
- [CU6] https://cursor.com/security

**GitHub Copilot**
- [G1] https://github.blog/changelog/2026-06-17-github-copilot-app-generally-available and https://www.helpnetsecurity.com/2026/06/08/github-copilot-app-ai-coding-agents/
- [G2] https://docs.github.com/en/copilot/concepts/copilot-surfaces/github-copilot-app
- [G3] https://github.blog/changelog/2026-02-26-github-mobile-track-coding-agent-progress-in-real-time-with-live-notifications
- [G4] https://github.blog/changelog/2026-10-28-work-from-anywhere-with-a-new-connected-copilot-cloud-experience/
- [G5] https://github.com/features/copilot/plans and https://github.blog/changelog/2026/04/27/github-copilot-is-moving-to-usage-based-billing/

**Warp**
- [W1] https://www.warp.dev/blog/universal-agent-support-level-up-coding-agent-warp
- [W2] https://docs.warp.dev/agent-platform/cloud-agents/orchestration/multi-agent-runs.md
- [W3] https://www.warp.dev/pricing

**Zed**
- [Z1] https://zed.dev/blog/parallel-agents and https://zed.dev/blog/zed-1-0
- [Z2] https://zed.dev/docs/ai/agent-panel and https://zed.dev/docs/ai/external-agents.md

**Devin**
- [D1] https://docs.devin.ai/release-notes/overview
- [D2] https://cognition.com/blog/introducing-devin-desktop
- [D3] https://docs.devin.ai/enterprise/security/enterprise-security
- [D4] https://devin.ai/pricing and https://cognition.com/blog/new-self-serve-plans-for-devin

**Others**
- [S1] Sculptor: https://imbue.com/sculptor/
- [N1] Nimbalyst (Crystal's successor): https://nimbalyst.com/pricing
- Claude Squad: https://github.com/smtg-ai/claude-squad
- [VK1] Vibe Kanban: https://vibekanban.com/blog/shutdown
- Terragon: https://github.com/terragon-labs/terragon-oss
- [O1] OpenCode: https://opencode.ai and https://github.com/anomalyco/opencode
- opcode: https://github.com/winfunc/opcode
- Jules: https://jules.google/docs/usage-limits
- Factory: https://factory.com/pricing and https://docs.factory.com/factory-app/overview
- [AM1] Amp: https://ampcode.com/pricing
- [HP1] Happy: https://happy.engineering/docs/security
- [HP2] Happy: https://apps.apple.com/us/app/6748571505
- [OM1] Omnara: https://www.omnara.com/blog
- [P1] Paradev (formerly Superset): https://paradev.com/pricing
- [K1] Kilo Code: https://kilo.ai/docs/code-with-ai/platforms/mobile
- Emdash: https://github.com/generalaction/emdash
- Antigravity 2.0: https://thenextweb.com/news/google-antigravity-2-desktop-cli-sdk-io-2026

**Not verified**
- Conductor's official prices, and whether Conductor Cloud survives.
- Whether herdr's phone apps push while closed.
- Copilot Business/Enterprise prices on GitHub's own page.
- The Codex default for steer versus queue.
- How Lazybox feels to anyone but its author.
