# Security policy

## Supported versions

Lazybox is an early preview. Security fixes are applied to `main`; there are no
maintained release branches.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability involving
credentials, command execution, the Runner's RPC or device gateway, phone
pairing, workspace or worktree isolation, or terminal output. Report it
privately, either way:

- GitHub's private security-advisory form,
  <https://github.com/AntoineToussaint/lazybox/security/advisories/new>, when
  the repository has private reporting turned on;
- or, always, email
  [toussaint.antoine@gmail.com](mailto:toussaint.antoine@gmail.com). Use it
  if the form is unavailable or you prefer email.

Include the affected commit, platform, reproduction steps, expected impact,
and any suggested mitigation. Do not include real provider tokens, account
credentials or private repository contents. You should receive an
acknowledgement within seven days; coordinated disclosure timing will be agreed
before publication.

## Security boundaries

- Agents run with the same operating-system identity and repository access as
  the Runner. Lazybox is not a sandbox.
- The Runner's gRPC and Connect listeners answer on loopback only and require
  a bearer. The owner's bearer is written to an owner-only `endpoint.json`;
  each agent execution receives its own narrower credential
  ([RESOURCE_OWNERSHIP.md](RESOURCE_OWNERSHIP.md#agent-credentials)). That
  narrower credential **prevents accidents, not a hostile agent**: see
  [What an agent can reach](#what-an-agent-can-reach).
- Remote phone access is off by default. When turned on, the device gateway is
  served over HTTPS on the owner's tailnet only, devices pair with a one-time
  QR code and hold revocable, scoped grants ([REMOTE.md](REMOTE.md)).
- Provider credentials stay in each account profile's isolated home; they are
  never placed in URLs, profile values or logs
  ([SUBSCRIPTIONS.md](SUBSCRIPTIONS.md)).

## What an agent can reach

The product rule is that values such as API keys and the owner's bearer never
leave the Runner. Today that holds against an agent that follows its tools,
not against one that sets out to break it.

An agent credential (`lbx-agent.…`, in `<state>/agent-endpoints/<id>.json`,
which `LAZYBOX_V2_ENDPOINT` names) reaches only the RPCs in `agentRPCs`
(`services/runner/code/internal/transport/auth.go`), for its own conversation,
commands and workspace. That stops an agent, or a script it runs, from
approving its own requests, answering its own questions or changing a setting
**by using the Lazybox CLI or the RPC it was given**. It does not stop an agent
that reads files: the agent runs as the same OS user as the Runner, and that
user can read every file the Runner keeps. From its own endpoint path an agent
finds the state folder, and in it:

| File in `<state>` | What it gives |
| --- | --- |
| `endpoint.json` | The owner's bearer: every RPC, including approving, answering, settings and deleting. |
| `agent-credential.key` | The HMAC key agent credentials derive from: a credential for any other conversation. |
| `device-credential.key` | The HMAC key device credentials derive from: a phone's credential for any paired device id. |
| `secrets/anthropic-api-key` | The saved Anthropic API key (also in the agent's own environment, see below). |
| `connections/` | The GitHub connection's access and refresh tokens. |
| `agent-endpoints/`, `terminal-endpoints/`, `hook-endpoints/` | Other agents', terminals' and hooks' scoped credentials. |
| `hosts/`, `command-hosts/`, `terminal-hosts/` | The session hosts' records and control sockets, through which another conversation's agent can be driven or stopped. |

So the remaining risk is: **an agent that misbehaves on purpose (a prompt
injection, a malicious dependency it runs) can take the owner's authority**,
and through it approve its own tool calls or start other agents. The
allowlist, the grants and the scoped files keep honest mistakes contained; they
are not a boundary between the agent and the person.

### What the harnesses can deny today

Checked, and deliberately not turned on:

- **Claude Code `Read(…)` deny rules** (the worker's `settings.permissions`,
  `workers/claude-sdk/src/options.ts`) are a real mechanism: they cover the
  Read, Grep and Glob tools and the file reads Claude Code can resolve
  statically in a Bash command. They are not a boundary (`cat "$(dirname
  "$LAZYBOX_V2_ENDPOINT")/../endpoint.json"`, a script, or any program the
  agent writes reads the file), and while any `Read()` deny rule is configured
  Claude Code asks for approval of every compound command that `cd`s and then
  reads a relative path (`cd repo && cat README.md`), which would add prompts to
  ordinary read-only and Ask conversations. A speed bump with that cost is not
  shipped; the fix below is.
- **Claude Code's sandbox** (`sandbox.enabled` with `filesystem.denyRead`)
  would confine Bash reads, but it also confines the network and the writes of
  every command, which changes what an agent can do in its workspace. It is
  the person's choice in their own Claude settings, not one Lazybox makes.
- **`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`** strips Anthropic credentials from
  the commands an agent runs, but forces the permission mode to default,
  which breaks plan, read-only and YOLO conversations.
- **Codex's sandbox** (`read-only`, `workspace-write`, `danger-full-access`)
  restricts writes and the network, not reads: there is no read deny to set.

### The saved Anthropic API key

With a key saved, a `claude-default` agent starts with `ANTHROPIC_API_KEY` in
its environment (`internal/accounts/apikey.go`, `WithAPIKey`), so every command
it runs inherits it. Lazybox removes any inherited `ANTHROPIC_BASE_URL`,
`CLAUDE_CODE_API_BASE_URL` and `CLAUDE_CODE_CUSTOM_OAUTH_URL` when it supplies
the key, so the key goes only to Anthropic or to a gateway the Runner selects
(which sets the base URL itself).

The follow-up keeps the key in the Runner: the agent talks to Anthropic through
a Runner-owned proxy (the metered gateway, `internal/runner/telemetry/gateway.go`,
already forwards `/v1/messages`) with a placeholder key, and the proxy replaces
`x-api-key` with the saved key on the way out, for that agent's route only. It
is not built yet because the route must survive a Runner restart: agents
outlive the Runner and reach the gateway through their session host's stable
proxy, so the injection belongs in the session host's proxy (with the key
handed to it over its private control connection, never its environment), or
the agent stalls whenever the Runner restarts. It also needs every
`claude-default` agent, not only metered ones, routed through it. Until then,
the key is as reachable as the files above.

## Design: credentials out of the agent's reach

Not built; this is the design the boundary above calls for. It has two parts,
and both are needed: one keeps the secrets off the filesystem, the other keeps
an agent from asking the Runner for them.

1. **No credential at rest that the user can read.** The Runner keeps the
   owner's bearer, `agent-credential.key`, `device-credential.key`, the API key
   and the GitHub connection in the macOS Keychain (on Linux, the Secret
   Service, or the kernel keyring), in items whose access control lists only
   the Runner's and the desktop app's code-signing identities. A process the
   agent starts, even as the same user, gets a Keychain prompt naming it rather
   than the value. `endpoint.json` keeps the URL and drops the bearer.
2. **The owner is identified by the connection, not by a bearer the agent
   could copy.** The owner's clients (the desktop app, the `lazybox` CLI the
   person runs) reach the Runner over a Unix socket in a `0700` folder. The
   Runner checks each connection's peer (`LOCAL_PEERPID`/audit token on macOS,
   `SO_PEERCRED` on Linux): the same user is necessary but not sufficient,
   because agents share it, so it also refuses a peer that descends from a
   session host's process group (the Runner knows every one), and on macOS
   requires the desktop app's code signature for owner-only RPCs. Agents keep
   their scoped credential over the loopback listener as today, which then
   accepts nothing else.

The complete answer is still process isolation: agents as a separate OS user,
or in a sandbox that cannot read the Runner's state. With the two parts above
an agent that misbehaves can still use its own credential and read the
workspace it works in, but no longer act as the person.
