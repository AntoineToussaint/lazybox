# Multiple subscriptions and work continuation

V2 must support several configured provider account/subscription profiles and continue work when the current profile has no usable capacity. Switching can use verified native resume or an explicit checkpoint handoff. Account availability and work identity are separate concerns.

This requirement applies from the first agent contract. Provider-specific account isolation, login, and resume mechanisms must be verified before implementation advertises them as supported.

The implementation preserves inherited native settings and credentials by
default. On 2026-10-29 the user explicitly requested once-only enrollment and
rotation among saved subscriptions. Rotation is now implemented behind an
explicit pool: `RunnerOptions.rotation_profile_ids` or configured
`defaults.rotationProfiles`. An empty pool leaves account selection unchanged.
No implicit provider change, global logout/login, or API-key fallback is used.

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
notifications do not trigger rotation. If no account is eligible, the failure
remains visible; this implementation does not run a timer to automatically
resume a blocked conversation when capacity returns.

Use **Settings → Subscriptions → Add subscription** in the main workspace,
or enroll on the Runner host using `lazybox account-login` as described in
[Running](RUNNING.md#enroll-subscriptions-once). Codex uses its native login
with an isolated `CODEX_HOME` and an explicitly selected file auth store.
Claude runs its native `auth login --claudeai` command with a private
`CLAUDE_CONFIG_DIR`. Lazybox opens the native browser URL; Claude owns the local
callback, code exchange and credential persistence. A fresh `auth status`
process must confirm `claude.ai`, a saved login and the expected config directory
before the account is registered. The native login is reused when switching.
Only permission and sandbox preferences are seeded from existing user settings;
auth and account metadata are not copied and the source settings are never written.
The CLI-only setup-token import remains available for automated provisioning.
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
and reset times. Only Runner-enrolled accounts appear here. Existing local
CLI sign-ins remain read-only references for session startup; Settings cannot
refresh or enroll into their stores. Viewing Settings does not start or stop
coding sessions. Refresh state is per subscription: one pending refresh disables
only its own button. Other accounts remain independently refreshable; errors
and previous observations remain scoped to the account requested.

Codex uses public app-server device sign-in RPCs and a correlated completion.
The provider page opens in the system browser only when requested. Successful
sign-in commits the profile into the live registry; cancellation does not.
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

Codex device sign-in requires the ChatGPT Security setting to be enabled. A
provider rejection naming that setting becomes an actionable enrollment error.
**Copy code** copies the exact one-time code through the native desktop clipboard
and confirms success; codes remain absent from persisted view state.

Enrollment is idempotent. Clients retain only a non-secret request reference
to recover lost acknowledgements. Tokens, one-time codes and sign-in URLs are
absent from saved client view state and ordinary Info metadata. Registered
profiles survive Runner restart; incomplete login processes do not. Completed
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

Capture the objective and criteria, accepted decisions, outstanding questions, completed results, source/base revisions, working-tree state, relevant staged/unstaged/untracked changes, retained command/output references, and running service references. Prefer stable references over copying large transcripts.

On the same Runner, preserve the existing working directory and inspect changes already made. Moving to another Runner additionally requires a verified transfer/reconstruction of required files and artifacts. Replaying the original prompt must not repeat edits or external side effects already performed.

A summary is a handoff, not an exact snapshot of private model state. Record provenance and any omitted or unavailable context. Pending approvals refer to particular provider requests; a successor cannot blindly replay an old approval. Reconcile command and provider-operation outcomes before retrying an uncertain action.

## Validation

The shared conformance suite simulates subscription exhaustion, temporary throttling, expired authentication, failed resume, interrupted switch, and competing profile selection. Verify one active writer, stable work identity, correct profile attribution, and accessible evidence after handoff.

Real-provider verification requires two usable configured profiles for a provider. Exercise both native resume where supported and checkpoint fallback, while a sibling session keeps its original profile. Record exact provider/adapter versions and actual capabilities. Missing profiles mean that path remains unverified; simulated capacity events do not establish that provider account switching works.
