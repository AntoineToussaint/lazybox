# Multiple subscriptions and work continuation

V2 must support several configured provider account/subscription profiles and continue work when the current profile has no usable capacity. Switching can use verified native resume or an explicit checkpoint handoff. Account availability and work identity are separate concerns.

This requirement applies from the first agent contract. Provider-specific account isolation, login, and resume mechanisms must be verified before implementation advertises them as supported.

The implementation preserves inherited native settings and credentials by
default. On 2026-10-29 the user explicitly requested once-only enrollment and
rotation among saved subscriptions. On 2026-11-17 automatic rotation left the
core ([CLAUDE_COMPLIANCE.md](CLAUDE_COMPLIANCE.md)): the core keeps several
accounts per provider, chosen explicitly per conversation, and switching by
itself at a limit is an extension's **account policy**
([EXTENSIONS.md](EXTENSIONS.md#account-policy)), off until the person adds and
allows one (Lazybox bundles *Subscription rotation*, not installed). Without a
policy (fresh state) a conversation's pool is empty, and a pool saved with a
conversation switches nothing: the rotation supervisor asks at every limit whether a policy
covers the provider. With one, a new conversation's pool is the policy's
ordered accounts, or every other signed-in subscription (enrolled, isolated
profile) of the same provider, sorted by ID and capped at 16. Two opt-outs
narrow it: **Settings → Subscriptions → Automatic switching** takes one account
out of every new conversation's pool (`RunnerSettings.rotation_excluded_profile_ids`),
and unchecking **Switch subscriptions automatically** when starting a
conversation keeps that one on its selected account (`RunnerOptions.no_rotation`,
saved with the conversation). An explicit `RunnerOptions.rotation_profile_ids`
or configured `defaults.rotationProfiles` still wins, and a configured empty
list disables the default. The pool is resolved when a conversation starts;
subscriptions added or excluded later apply to conversations started afterwards,
and a resumed conversation keeps its saved pool. An empty pool leaves account
selection unchanged. No implicit provider change, global logout/login, or
API-key fallback is used.

Claude is different ([CLAUDE_COMPLIANCE.md](CLAUDE_COMPLIANCE.md)). The
recommended credential is an Anthropic API key the person saves in Settings →
Agents & workspaces → Claude (`SetClaudeApiKey`, kept in
`<state>/secrets/anthropic-api-key`, `0600`): the `claude-default` account's
agents get it as `ANTHROPIC_API_KEY`, every other Claude credential removed.
Without one, `claude-default` runs on the person's own Claude Code sign-in.
Claude subscriptions saved through Lazybox's own sign-in start only with
`RunnerSettings.claude_subscription_sign_in` on (off by default), and Lazybox
imports no Claude subscription token.

## Implemented local Runner path

`internal/runner/rotation` supervises provider processes inside the same Engine
and execution ID. Account failures arrive as structured provider events. On
subscription exhaustion, throttling, or authentication failure it selects an
eligible isolated profile of the same provider, cancels the old attempt,
confirms native process exit, then opens a fresh native conversation with a
bounded portable checkpoint. Model, explicit permission selection, gateway,
Collection lease, command IDs, operation receipts, and queued messages stay
with the execution. This is a handoff, not cross-account native resume.

The checkpoint contains public conversation text, retained command metadata,
and current Collection HEAD/status observations. Missing/truncated source
facts are explicit. The successor must inspect current work before acting;
completed edits and external actions are not replayed. Images are not copied
into the text checkpoint. Old native approvals and child IDs are scoped to
an attempt and cannot be reused by a replacement process.

Account availability is retained under the configured Runner state directory.
Native reset times permit a later probe. A throttled account without a native
reset receives a 60-second probe delay; exhaustion with no known reset stays
unavailable until a subsequent provider observation. Unknown errors, context
limits, local turn/budget limits, billing problems, and retry-in-progress
notifications do not trigger rotation. If no account is eligible, the
conversation stops at the limit and says so; the Runner itself runs no timer to
resume it when capacity returns.

### Switching on request

The owner can move a running conversation at any time (`SwitchSubscription`,
owner only; agent and terminal credentials are refused). This explicit switch
is core and needs no account policy; `enable_rotation` does. It uses the same
attempt mechanism as automatic rotation, in the same execution: the old native
process is closed before the new one starts, credentials are resolved first so
a profile that cannot start never costs the running agent, and the new account
starts from the same portable checkpoint. An interrupted request (the
conversation stopped at a limit, or a turn is running) is continued at once; an
idle conversation starts no turn, and the checkpoint travels with the next
message instead (durable across a Runner restart). Choosing the conversation's
current subscription after its reset continues the same native conversation in
place. The person may pick a subscription whose last observation is a limit:
a refusal comes back as a limit again.

At an account limit (exhausted, throttled, sign-in or billing) a conversation
still accepts messages: they are journaled once, wait in the queue and are
delivered after the switch or the retry, after the interrupted request. No
operation is replayed. Other faults still refuse messages until reconciled.

`SetSessionRotation` (refused without an account policy when turning it on)
turns automatic switching on or off for a running conversation (its pool is resolved as for a new conversation on its current
subscription, honouring Settings exclusions, and saved with it). With
`all_sessions` it applies to every running conversation of the provider that
did not opt out when it started, and turning it on takes the provider's
subscriptions back into automatic switching in Settings.

In the conversation, an automatic switch is one quiet line ("Switched to Work ·
Personal is back at 6:00 PM"). Without one, a single card names the limit and
when the subscription is back ("back at 6:00 PM (in 2h 10m)"), offers **Switch
to** the best other subscription first (a picker when there are several, with
each one's state and reset) and **Wait for reset**. Only while an account
policy covers the provider does it also offer **Always switch automatically**
(the first time, one question offers it for every conversation). Waiting continues the request on the same subscription
at the reset while the conversation is open in a window, or the next time it is
opened; this is the client's timer, not the Runner's. The session ⋯ menu, the
subscription in the conversation's header and `/subscription` open **Switch
subscription…** at any time. Provider tokens are never shown.

Every list of subscriptions has one order (`subscription-groups.ts`, with
`orderSubscriptions` in `subscription-model.ts`): grouped by provider (Claude,
Codex, then each ACP agent by name) under a small header with its icon, and
within a group the current one first, then the available ones, then those not
known to be limited, then the limited ones, by name within each. One group
shows no header. That is Settings → Subscriptions, the switch dialog and the
limit card (the conversation's own provider only: a conversation cannot move
across providers), and the account pickers of New conversation, New task, a
start request and Missions (`<optgroup>`s when they mix providers); a limited
one says when it is back.

Every surface says when a limited subscription can be used again the same way
(`backAt`/`comeback` in `subscription-model.ts`): the conversation card and
header, the switch picker, the Inbox line, and **Settings → Subscriptions**. A
limit lasts until every usage window at 100% has reset, so a card is back at the
latest of those resets ("Back at 6:00 PM (in 2h 10m)"), updated each half
minute. Once that time has passed the report is stale: the card says "Should be
available again — checking…", refreshes that subscription's usage once by
itself, and shows what the provider reports now. A window whose reset has passed
shows "Reset" instead of a stale percentage. With no reported reset time the
card says so. An available card names only its next reset, of its most-used
window.

Opening **Settings → Subscriptions** refreshes usage for each managed
subscription with a private sign-in. The page shows the last report while
the provider is contacted, and an individual card can still be refreshed.
The conversation’s limit card also has **Refresh** to check the current
provider’s managed subscriptions and update the available choices.

Use **Settings → Subscriptions → Add subscription** in the main workspace,
or enroll a Codex account on the Runner host using `lazybox account-login` as
described in [Running](RUNNING.md#enroll-subscriptions-once). Codex uses its
native login with an isolated `CODEX_HOME` and an explicitly selected file auth
store. Claude, only with Claude sign-in in Lazybox on, runs its native
`auth login --claudeai` command with a private `CLAUDE_CONFIG_DIR`. Lazybox opens the native browser URL; Claude owns the local
callback, code exchange and credential persistence. A fresh `auth status`
process must confirm `claude.ai`, a saved login and the expected config directory
before the account is registered. The native login is reused when switching.
Only permission and sandbox preferences are seeded from existing user settings;
auth and account metadata are not copied and the source settings are never written.
The setup-token import is refused for Claude (Lazybox may not store a Claude.ai
credential). An account imported that way earlier keeps working while Claude
sign-in in Lazybox is on, marked *Imported token; not recommended* with
**Remove** in Settings. A Runner state that already had Claude subscriptions
saved, or automatic switching on, keeps them working: the migration carries
Claude sign-in in Lazybox over on and installs and approves Subscription
rotation ([CLAUDE_COMPLIANCE.md](CLAUDE_COMPLIANCE.md#existing-setups-keep-working)).
Settings reloads the registry live; no Runner restart is required. Existing
profiles are never overwritten. Credentials stay out of profile metadata,
journals, RPC responses and checkpoints.

Managed enrollment can live outside execution state through `subscriptionState`.
Working and Dev can [select one existing account store](ENVIRONMENTS.md#share-saved-subscriptions-between-working-and-dev),
preserving provider home paths and saved native logins without copying tokens.
New registrations are visible to both Runners and survive either Runner's
restart. Session history and quota observations remain per Runner; refreshing
usage queries the same saved provider account from each environment.

Tokens can expire or be revoked; ordinary account rotation does not re-login,
but invalid credentials still require a human to renew them. The default/shared Claude Keychain login is never selected for a managed
account. Native cross-account resume remains unverified and is not used. Two paid subscriptions and their
live reset/rotation behavior have not been tested in this environment.

Codex handoff carries the original native model and approval/sandbox settings.
Custom named permission profiles cannot be reconstructed from the legacy
sandbox response alone. Their automatic handoff is refused rather than
substituting another account's default policy.

## Settings and provider usage

The main workspace Settings page starts with a Subscriptions tab. It lists
named profiles, native availability, provider plan when supplied, usage windows
and reset times. Only Runner-enrolled accounts appear here as subscription
cards. Under them, ACP agents that share one account are grouped
(`SharedCredentials.tsx`). Today that is **Google**: Gemini CLI and
Antigravity. Antigravity uses Gemini CLI's `GEMINI_API_KEY` when it has none
of its own ([ACP.md](ACP.md#providers-and-profiles)). For each agent the group
shows where its key comes from, by name only, plus the command that signs it
in on the Runner. It shows no usage, because neither agent reports any. Existing local
CLI sign-ins remain read-only references for session startup; Settings cannot
refresh or enroll into their stores. Viewing Settings does not start or stop
coding sessions. Refresh state is per subscription: one pending refresh disables
only its own button. Other accounts remain independently refreshable; errors
and previous observations remain scoped to the account requested.

Codex signs in through public app-server RPCs in the profile's own
`CODEX_HOME` with the file credential store, in one of two ways
(`AddSubscriptionRequest.sign_in_method`, echoed on `SubscriptionEnrollment`):

- **Browser** (the default): `account/login/start {"type":"chatgpt"}` returns
  `{loginId, authUrl}` and no code. The client opens `authUrl` through the
  host's sign-in opener; ChatGPT redirects to Codex's own loopback listener
  (`127.0.0.1:1455`, or Codex's fallback port when that one is held), and
  Codex writes the credentials into the profile. It therefore works only from
  a browser on the Runner's computer. Cancel sends `account/login/cancel
  {loginId}` before the app-server is closed, freeing the port.
- **Device** (`"device"`): `account/login/start {"type":"chatgptDeviceCode"}`
  returns a verification URL and a one-time code to enter on any device.

Both complete with the same `account/login/completed` notification, matched by
`loginId`; other notifications (such as `remoteControl/status/changed`, which
can precede a response) are skipped. Verified against codex-cli 0.169.0
(`TestRealCodexBrowserSignInStartsAndCancels` starts and cancels a browser
sign-in in a scratch `CODEX_HOME`). The client starts with the browser when
its page is served from loopback or the desktop app's origin (the owner's
windows, on the Runner's computer) and with a code otherwise; either way the
person can switch, which cancels the waiting sign-in and starts the other for
the same account. Claude has only the browser and refuses `"device"`.
Successful sign-in commits the profile into the live registry and closes the
dialog; cancellation registers nothing.
Refresh uses `account/read` and `account/rateLimits/read` without starting a
thread or model turn. Separate native quota buckets remain separate.

Claude enrollment opens its provider-owned browser flow directly from Settings.
There is no terminal instruction or token field. The native callback currently
requires the browser and Runner on the same machine; remote enrollment needs
its own supported callback transport. Quota windows arrive from structured SDK
`rate_limit_event` reports during activity and from **Refresh usage**. Refresh
uses SDK 0.3.311's experimental
`usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET` method with
`skipBehaviors: true`, no prompt, no tools/hooks and no persisted conversation.
This version-pinned adapter keeps 0–100 usage percentages separate from event
fractions. Missing windows stay unavailable; failed refreshes retain prior
observations. Primary allowance exhaustion marks the profile exhausted unless
the provider reports extra usage enabled; model-specific limits remain separate.
The experimental method may change on an SDK upgrade and must be revalidated.
Live queries on two isolated saved accounts returned weekly quota successfully.
The earlier `/usage` slash-command probe reported session costs, but did not
establish that the SDK lacked an independent quota method.

Codex device sign-in requires the ChatGPT Security setting that allows device
codes; in a Business or Enterprise workspace only an admin can enable it. A
provider rejection naming that setting fails the enrollment with
`error_code: "device_sign_in_disabled"` and one line saying so; the client
offers **Sign in with the browser instead**. When Codex cannot bind its
callback port, the browser enrollment fails with
`error_code: "sign_in_in_progress"`, and the client offers a code instead.
Before that, Codex itself sends `GET /cancel` to whatever holds 1455 (meant
to stop a `codex login` waiting there) and then falls back to 1457; it
fails ("failed to start login server: Port 127.0.0.1:1457 is already in
use") only when both are held. Both behaviours were observed with codex-cli
0.169.0 and a scratch `CODEX_HOME`.
**Copy code** copies the exact one-time code through the native desktop clipboard
and confirms success; codes remain absent from persisted view state.

Enrollment is idempotent. Clients retain only a non-secret request reference
to recover lost acknowledgements. Tokens, one-time codes and sign-in URLs are
absent from saved client view state and ordinary Info metadata. Registered
profiles survive Runner restart; incomplete login processes do not. A sign-in
still waiting after 10 minutes is stopped and reads `expired` ("The sign-in
took too long and was stopped"), as does one a restarted Runner no longer
has; one whose account was saved reads `added`. Completed
requests can be reconciled from saved metadata without replaying a token.
Credential renewal/removal UI and distributed Hub account management remain
subsequent work.

## Core records and ownership

An account profile identifies a configured provider account/subscription context. Record its provider, visible label, credential reference, demonstrated capabilities, availability observations, and selection policy. Several credentials may represent the same subscription; profile count must not be treated as independent capacity without account identity evidence.

A participant is a human or agent. An account profile is the execution credential context. Preserve who requested work and who owns it when the account profile changes. Do not impersonate another participant by switching subscriptions.

Each execution attempt is pinned to a profile and records which profile was actually used. In a future distributed deployment the Hub owns shared eligibility and availability. The current local Runner owns the explicit pool and resolves the selected profile's credentials, starts the provider context, and reports execution facts. Credential material stays out of ordinary work events, logs, checkpoints, and UI state.

`internal/accounts` owns the profile and selection vocabulary. Runner adapters implement verified provider isolation and continuation; `internal/runner/checkpoints` owns explicit handoff records. Generic policy, queueing, and history remain outside individual provider adapters.

## Detecting unavailable capacity

Distinguish subscription exhaustion, a temporary rate limit, invalid or expired authentication, a model restriction, a transient provider failure, and unknown failure. Preserve the provider evidence and confidence of the classification. An arbitrary failure must not trigger a fleet-wide account rotation.

Policy can select another eligible profile, wait for capacity to return, or request a human decision. A work item waiting for subscription capacity remains visible and retains its assignment and evidence. Account availability observations include their time and any known retry/reset time.

Selection respects configured eligible profiles, provider/model capabilities, available concurrency, and the authorizing participant's scope. The UI shows waiting, switching, resumed, transferred, and failed states separately.

## Continuation order

1. Record why the current profile cannot continue and reconcile the current turn and pending operations.
2. Select an eligible replacement profile through policy or an explicit human choice.
3. Establish that the predecessor is stopped or otherwise fenced from conflicting writes and integration. A timeout does not prove that a process stopped.
4. Attempt native continuation only when the provider integration has demonstrated resume under the replacement credential context.
5. If native continuation is unsupported or fails, prepare an explicit checkpoint and start a successor session with that checkpoint and access to retained artifacts and output.
6. Record the successor account/session/attempt and whether the continuation was native resume or a handoff. Keep the same shared work identity.

Changing a provider login may leave its native conversation resumable; it is a provider-specific behavior to test. Native session IDs may be scoped to an account, local profile directory, machine, or provider version. Do not assume portability merely because a session file exists.

## Credential isolation

Enrollment creates new directories under the configured Runner state root.
Codex pins `cli_auth_credentials_store="file"` for login, quota reads and agent
execution, avoiding a shared system credential store. Claude uses a private native subscription login (or an explicitly imported
token). Login/logout commands are disabled in coding processes; only the
separate enrollment subprocess enables login. Native `BROWSER` integration
hands the provider-generated URL to the client through a temporary private
file, which is removed after reading and on cancellation. Raw CLI auth output
is discarded. Inherited auth environment variables are
removed only from those child environments. The Runner does not set global
environment variables, rewrite shell configuration, or run global login/logout.
Local auth directory aliases, including symlinks, are rejected.

Claude documents separate config directories for claude.ai accounts, but
[Console keyless sign-ins are an exception](https://code.claude.com/docs/en/authentication#log-in-with-multiple-accounts).
This integration uses subscription tokens; it does not enroll Console keyless
accounts. Tests verify unchanged canary auth/config files and modes, explicit
private native-login writes, and refusal to refresh inherited local accounts.
Actual macOS Keychain mutation monitoring and live multi-account login remain
unverified.

Pin a session to its own verified credential/configuration context. Switching one execution must not silently log out, redirect, or invalidate sibling executions using another profile. A global logout/login is not a concurrency-safe implementation unless the integration proves isolation or explicitly serializes affected use.

Record support per adapter: isolated concurrent profiles, serialized switching, or unsupported. Report limitations to scheduling and UI. Test switching one profile while another execution remains active.

## Checkpoint contents

When every Claude subscription is limited, **Continue in a new Codex
conversation** opens Codex in the same workspace and sends a text handoff
with prior requests, agent progress, and queued follow-ups. For long
conversations, it keeps the opening request and prioritizes recent context.

Capture the objective and criteria, accepted decisions, outstanding questions, completed results, source/base revisions, working-tree state, relevant staged/unstaged/untracked changes, retained command/output references, and running service references. Prefer stable references over copying large transcripts.

On the same Runner, preserve the existing working directory and inspect changes already made. Moving to another Runner additionally requires a verified transfer/reconstruction of required files and artifacts. Replaying the original prompt must not repeat edits or external side effects already performed.

A summary is a handoff, not an exact snapshot of private model state. Record provenance and any omitted or unavailable context. Pending approvals refer to particular provider requests; a successor cannot blindly replay an old approval. Reconcile command and provider-operation outcomes before retrying an uncertain action.

## Validation

The shared conformance suite simulates subscription exhaustion, temporary throttling, expired authentication, failed resume, interrupted switch, and competing profile selection. Verify one active writer, stable work identity, correct profile attribution, and accessible evidence after handoff.

Real-provider verification requires two usable configured profiles for a provider. Exercise both native resume where supported and checkpoint fallback, while a sibling session keeps its original profile. Record exact provider/adapter versions and actual capabilities. Missing profiles mean that path remains unverified; simulated capacity events do not establish that provider account switching works.

When Codex is installed as a symlink to a package that includes
`codex-code-mode-host`, Lazybox launches the package executable directly so
Codex can find the helper beside it. An already running Codex execution
keeps the executable it started with.

### Agent version updates

Lazybox checks official stable Claude Code and Codex releases on the Runner
when the workspace opens and about every six hours while it is open.
**Settings → Agents & workspaces → Agent updates** shows installed and
latest versions and links to update instructions. Bundled Claude Code is
updated through Lazybox. Version checks send no account details and do not
install updates or restart agents.

In Settings → Subscriptions, filter cards by their reported status. Sort within each provider by availability, account name, or earliest limit reset. Accounts without a reported limit reset follow timed resets.
