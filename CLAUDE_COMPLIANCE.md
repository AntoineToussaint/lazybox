# Claude: what Anthropic's terms allow, and what Lazybox does

Lazybox runs Claude through the Claude Agent SDK (`workers/claude-sdk`), which
starts the Claude Code program (`claude`) and drives it. This page writes down
what Anthropic's own pages say about that, how Lazybox maps to each rule, and
what changed to follow them. It was prompted by the competitive audit's
compliance finding ([audit §4, item 5](audits/2026-11-17-competitive/README.md#4-where-we-are-behind-ranked-by-how-many-users-it-costs)).

Every quote below was read on **2026-11-17** on the page linked. **Quote** means
Anthropic's wording; **Reading** means our interpretation, which Anthropic has
not confirmed. This is not legal advice. When a page changes, this page is
stale until someone reads it again.

## The rules, quoted

### R1. No claude.ai login in third-party products built on the Agent SDK

[Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) (also in
the [quickstart](https://code.claude.com/docs/en/agent-sdk/quickstart)):

> Unless previously approved, Anthropic does not allow third party developers to
> offer claude.ai login or rate limits for their products, including agents
> built on the Claude Agent SDK. Use the API key authentication methods
> described in the Quickstart instead.

The quickstart's methods: `ANTHROPIC_API_KEY`; Amazon Bedrock
(`CLAUDE_CODE_USE_BEDROCK=1` with AWS credentials); Claude Platform on AWS
(`CLAUDE_CODE_USE_ANTHROPIC_AWS=1`); Google Cloud (`CLAUDE_CODE_USE_VERTEX=1`);
Microsoft Foundry (`CLAUDE_CODE_USE_FOUNDRY=1`).

### R2. Products use API keys; nobody else's code handles a Claude sign-in

[Legal and compliance → Authentication and credential use](https://code.claude.com/docs/en/legal-and-compliance):

> **OAuth authentication** is intended exclusively for purchasers of Claude Free,
> Pro, Max, Ultra, Team, and Enterprise subscription plans and is designed to
> support ordinary use of Claude Code and other native Anthropic applications.

> **Developers** building products or services that interact with Claude's
> capabilities, including those using the Agent SDK, should use API key
> authentication through Claude Console or a supported cloud provider. Anthropic
> does not permit third-party developers to offer Claude.ai login into their own
> applications, or to route requests through Free, Pro, Max, or Ultra plan
> credentials on behalf of their users. Moreover, developers may not collect,
> store, or intermediate Claude.ai credentials or session tokens — sign-in to a
> Claude account must complete through Anthropic's own flow.

### R3. The exception: a person's own sign-in to the unmodified binary

Same section:

> This does not restrict how customers provision and manage their own API keys
> or third-party inference provider credentials […] provided the resulting usage
> is billed to the key owner […]. Nor does it prevent an end user from signing
> in to the unmodified Claude Code binary with their own Claude subscription,
> including where a platform hosts Claude Code as described under *Can customers
> offer Claude Code in their products?* above.

> Anthropic reserves the right to take measures to enforce these restrictions
> and may do so without prior notice.

### R4. Shipping or running Claude Code inside a product

[Legal and compliance → Can customers offer Claude Code in their products?](https://code.claude.com/docs/en/legal-and-compliance):

> Unless we've mutually agreed otherwise, preinstalling or running Claude Code in
> your products or services (e.g. in hosted sandboxes or other agent
> infrastructure) requires agreeing to our Commercial Terms of Service and
> complying with the conditions below:
>
> - **The Claude Code binary must not be modified.** Claude Code must be
>   installed and run as published by Anthropic, and customers may not remove,
>   disable, or restrict any authentication method built into it (including
>   methods that permit signing in with a Claude account or the user's own API
>   key).
> - **Customers may not pay for, resell, or intermediate Claude usage on their
>   end users' behalf.** Each end user must authenticate with their own Anthropic
>   API key, Claude subscription plan credentials, or 3P inference provider
>   credential […].

The licence that ships with the program and the SDK
([`LICENSE.md`](https://github.com/anthropics/claude-code/blob/main/LICENSE.md),
and the npm packages' `LICENSE.md`):

> © Anthropic PBC. All rights reserved. Use is subject to the Legal Agreements
> outlined here: https://code.claude.com/docs/en/legal-and-compliance.

The SDK's `package.json` pulls the program in as a per-platform optional
dependency (`@anthropic-ai/claude-agent-sdk-darwin-arm64`).

### R5. Names

Agent SDK overview: allowed are "Claude Agent", "Claude" inside a menu already
labelled Agents, and "{YourAgentName} Powered by Claude"; not "Claude Code" or
"Claude Code Agent" as your product's name, nor Claude Code-styled art. Legal
and compliance: "You can accurately say, in plain text, that your product has
Claude Code preinstalled or that it runs Claude Code."

### R6. Ordinary, individual use; no shared accounts; no bypassing limits

[Legal and compliance → Usage policy](https://code.claude.com/docs/en/legal-and-compliance):

> Advertised usage limits for Pro, Max, and Ultra plans assume ordinary,
> individual usage of Claude Code and the Agent SDK.

[Consumer Terms](https://www.anthropic.com/legal/consumer-terms) (effective
Oct 8, 2025), which govern Free, Pro, Max and Ultra:

> You may not share your Account login information, Anthropic API key, or
> Account credentials with anyone else or make your Account available to anyone
> else.

> [You may not…] Except when you are accessing our Services via an Anthropic API
> Key or where we otherwise explicitly permit it, to access the Services through
> automated or non-human means, whether through a bot, script, or otherwise.

> […] or bypassing any of our systems or protective measures.

The Consumer Terms call per-plan caps "Technical Limitation[s]". The
[Usage Policy](https://www.anthropic.com/legal/aup) (effective Nov 12, 2026),
"Do Not Abuse Our Platform":

> Coordinate malicious activity across multiple accounts, or create multiple
> accounts, to avoid detection, circumvent product guardrails, or generate
> identical or similar inputs that otherwise violate our Usage Policy

> Intentionally bypass or interfere with capabilities, restrictions, or
> guardrails established within our products, including by circumvention of the
> account sign-up or log-in process […] without prior authorization from
> Anthropic

### R7. Anthropic's own description of plan usage through the SDK

[Use the Claude Agent SDK with your Claude plan](https://support.claude.com/en/articles/15036540)
(updated 2026-10-30):

> Nothing changes for your plan: when you're signed in with your Claude plan,
> the Claude Agent SDK and claude -p still draw from your subscription limits.

> For now, nothing has changed: Claude Agent SDK, claude -p, and third-party app
> usage still draw from your subscription limits.

### What none of these pages says

- Whether an app that drives the person's own, unmodified `claude` through the
  Agent SDK counts as "an end user signing in to the unmodified Claude Code
  binary" (R3), or as "offering claude.ai login or rate limits" (R1).
- Whether a desktop app may ship the SDK's copy of the program. R4's examples
  are hosted sandboxes and agent infrastructure.
- Anything about one person using several of their own subscriptions, or
  switching between them at a limit. There is no "one account per person"
  clause, and the Usage Policy's multiple-accounts clause names avoiding
  detection and guardrails, not usage limits.

The older wording "OAuth tokens from Free/Pro/Max are only for Claude Code and
claude.ai; using them in any other product, including the Agent SDK, is not
permitted" is **not** on the current page. Only press reports it, as a February
2026 change ([The Register](https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access)).
We do not rely on it.

## How Lazybox mapped to each rule (before this change)

| What Lazybox did | Rule | Status |
|---|---|---|
| **Settings → Subscriptions → Add subscription → Claude** started `claude auth login --claudeai` from Lazybox's own window, so that agents built on the Agent SDK ran on that claude.ai login. | R1, R2 | **Reading: not allowed without approval.** The sign-in itself completes in Claude Code's own flow (Claude Code receives the callback and stores the credential; Lazybox never reads it), but Lazybox's product is what offers the login. |
| `lazybox account-login --provider claude --token-stdin` copied a subscription setup-token into Lazybox's state and passed it as `CLAUDE_CODE_OAUTH_TOKEN`. | R2 ("collect, store, or intermediate") | **Quote-level: not allowed.** |
| The built-in `claude-default` account ran on whatever the person's own `claude` was signed in with (they signed in themselves, outside Lazybox). | R1 vs R3 | **Ambiguous.** Lazybox offers no login, and the program is unmodified, but the agent is "built on the Agent SDK" and draws on the plan's limits. |
| Automatic switching to another signed-in subscription when one reached its limit, **on by default** for every conversation. | R6 | **Reading: risky.** Switching past a plan's Technical Limitation plausibly reads as "bypassing […] protective measures". Not stated by Anthropic. |
| Several subscriptions per provider (personal, work), chosen per conversation. | R6 | **Reading: fine.** Each account is the person's own; nothing is shared and no limit is worked around when the person picks one. |
| No API key option in the app (only through a config file's `envRefs`). | R1, R2 | **Missing**: the method Anthropic asks products to use. |
| The release DMG ships the SDK's per-platform package, which contains the `claude` program, unmodified. | R4 | **Ambiguous**: "preinstalling Claude Code in your products" needs the Commercial Terms; distribution rights for a desktop app are not stated. |
| Agent processes on a saved subscription run with `DISABLE_LOGIN_COMMAND=1` and `DISABLE_LOGOUT_COMMAND=1`, so one account's agent cannot sign another account in or out. | R4 ("may not […] restrict any authentication method") | **Ambiguous**: these are Claude Code's own published settings, and the program is not modified; it is still a restriction. |
| UI says "Claude" in agent menus; "Claude Code" only to name Anthropic's program. | R5 | Fine. |
| Usage refresh reads the plan's quota through the SDK's experimental usage call. | — | No rule names it. |

## The evaluation: SDK, or the `claude` program directly?

The brief asked whether the worker should stop using the SDK and drive
`claude -p --input-format stream-json --output-format stream-json` itself, so
that Lazybox runs "the unmodified Claude Code binary" rather than "an agent
built on the Agent SDK".

**Finding: it would change the words, not the substance, so we do not do it.**

- The SDK already does exactly that: it starts the unmodified `claude` program
  (`pathToClaudeCodeExecutable`, or the copy in its platform package) with the
  stream-JSON flags and talks to it over stdin and stdout. The SDK overview
  calls itself "a library that runs the Claude Code binary". The credential,
  the program and the requests are identical either way.
- The rules turn on **who offers the login** (R1, R2) and **whether the program
  is run as published** (R3, R4), not on which library writes the JSON.
  Replacing the SDK with our own client of the same protocol to step around
  "built on the Agent SDK" would be the kind of workaround these rules exist to
  stop.
- It would also cost fidelity for nothing: approvals, questions and steering
  use the SDK's control protocol (`canUseTool`, `priority: 'next'`,
  `setPermissionMode`, `stopTask`), which is not documented as a public CLI
  interface and would have to be re-implemented and kept in step by hand.

So the question is not technical. It is **ambiguous under R1 vs R3**, and the
brief's rule for that case applies: gate it, explain it in the app, and ask
Anthropic.

## What changed

1. **An Anthropic API key is the recommended way** (R1, R2). Settings → Agents
   & workspaces → **Claude** → *Use an Anthropic API key*. The Runner keeps the
   key in its state folder (`<state>/secrets/anthropic-api-key`, mode `0600` in
   a `0700` folder; the Runner has no Keychain storage to reuse), never returns
   it (only its last four characters), and gives it to the `claude-default`
   account's agents as `ANTHROPIC_API_KEY`, removing the other Claude
   credentials from their environment. Each Runner (Working, Dev) keeps its
   own. A subscription token pasted as a key
   (`sk-ant-oat…`) is refused. Bedrock, Vertex, Foundry and Claude Platform on
   AWS work by setting their variables in the Runner's environment; Settings
   says which one is in effect. Onboarding recommends the API key.
2. **Lazybox no longer offers a Claude sign-in by default** (R1, R2). Adding a
   Claude subscription in Settings, and starting or switching a conversation
   onto one saved there, need **Settings → Agents & workspaces → Claude →
   Claude sign-in in Lazybox**, which is off for new state. Its description
   says why, in a few lines, with a link to the terms; on, a warning stays
   under it. Codex subscriptions are unaffected: OpenAI documents Sign in with
   ChatGPT for third-party apps.
3. **The person's own Claude Code sign-in still works** (R3), labelled for what
   it is: the `claude` program they installed and signed in to themselves, run
   unmodified. Lazybox shows no login for it. This is the case we ask Anthropic
   about.
4. **The setup-token import is refused for Claude** (R2). `lazybox
   account-login --provider claude` says why and points at the API key.
   Accounts imported before keep working while Claude sign-in in Lazybox is on;
   Settings → Subscriptions marks each *Imported token; not recommended*, with
   **Remove** (`RemoveSubscription`, which deletes its folder and token).
5. **Automatic switching left the core** (R6). Several accounts per provider
   stay, chosen explicitly per conversation; the limit card offers *Switch to
   <other account>* as a manual action. Switching by itself at a limit is now an
   **account policy** of an extension ([EXTENSIONS.md](EXTENSIONS.md#account-policy)),
   and Lazybox's own *Subscription rotation* extension is bundled but neither
   installed nor approved for new state. Approving it for Claude shows a plain
   warning with a link to the terms.
6. **Releases can leave the program out** (R4). `make release
   RELEASE_FLAGS=--without-claude-code` (or `LAZYBOX_RELEASE_CLAUDE_CODE=omit`)
   builds without the SDK's platform package. The Runner then uses the `claude`
   on its PATH, and the app offers **Install Claude Code**, which runs
   Anthropic's installer (`curl -fsSL https://claude.ai/install.sh | bash`) in
   the shell panel, where the person sees and runs it.

### Existing setups keep working

Compliant defaults apply to new Runner state; a setup that already existed
when this version first runs keeps working (`internal/runner/account_policy.go`,
once per state):

- **Claude subscriptions already saved in Lazybox** (signed in, or imported
  from a token): Claude sign-in in Lazybox is carried over **on** for that
  state (`RunnerSettings.claude_sign_in_carried_over`), so they keep starting,
  and one notice says: *Lazybox now follows Anthropic's terms for Claude
  sign-in. Your saved Claude accounts keep working; review in Settings → Agents
  → Claude*, with a link to the terms. Only new token imports are refused.
- **Automatic switching that was on** (a signed-in account not excluded,
  beside another account of its provider, or a configured pool): the bundled
  *Subscription rotation* extension is installed and approved for that state,
  with the same settings (exclusions and configured pools stay), for Codex and
  Claude as before, and one notice says so. It is not silently turned off.
- **Fresh state** gets the compliant defaults: sign-in off, no extension.

This keeps the people already depending on it working, which is the
owner's call: those carried-over paths remain the ones this page reads as not
allowed without Anthropic's approval (R1, R2, R6). The person can turn either
off in Settings; the migration never turns them back on.

## Decisions left to the owner

- **Ship the program in public releases, or not.** The omit path exists either
  way. Until Anthropic confirms that a desktop app may preinstall it under the
  Commercial Terms, the cautious default is to omit it.
- **Whether to keep the person's-own-sign-in path ungated** (change 3) until
  Anthropic answers.
- **`DISABLE_LOGIN_COMMAND` in saved-subscription agents.** Kept as it is on
  the gated path; removing it would let one account's agent sign another out.
- **How long carried-over state stays on.** Existing setups keep Claude
  sign-in in Lazybox and Subscription rotation on until the person turns them
  off; a later release could ask again once Anthropic answers.

## Questions to ask Anthropic

1. Lazybox is a local desktop app. It drives the `claude` program the person
   installed, unmodified, through the Agent SDK. If the person signed in to that
   program themselves (outside our app), is that "an end user signing in to the
   unmodified Claude Code binary with their own Claude subscription"?
2. May our window start Claude Code's own `claude auth login --claudeai` (the
   browser flow completes on claude.ai; Claude Code stores the credential in a
   config folder our Runner creates; we never read it), or is that "offering
   claude.ai login"?
3. May one person use several of their own Claude subscriptions (for example
   personal and work), chosen explicitly per conversation? And may software
   move a conversation to another of their subscriptions when one reaches its
   usage limit, if the person opted in?
4. May a desktop app ship the SDK's platform package (the unmodified `claude`
   program) inside its signed app bundle, and under which terms?
5. Is setting `DISABLE_LOGIN_COMMAND` / `DISABLE_LOGOUT_COMMAND` for agents on a
   saved account "restricting an authentication method"?

## Where it lives

| Where | What |
|---|---|
| `internal/accounts/apikey.go` | The API key's file, its checks, and the environment it gives |
| `internal/runner/claude_auth.go` | Which Claude credential an agent gets; the sign-in gate |
| `internal/runner/account_policy.go` | Account policies from extensions; the one-time notices |
| `internal/power/extensions.go` | `accountPolicy:` in a manifest, the `switch:accounts` grant |
| `internal/power/extensions/examples/subscription-rotation/` | The first-party *Subscription rotation* extension |
| `scripts/release/build.mjs` | `--without-claude-code` |
| `packages/surfaces/src/ClaudeAuthSettings.tsx` | Settings → Agents → Claude |
