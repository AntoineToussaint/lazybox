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
  ([RESOURCE_OWNERSHIP.md](RESOURCE_OWNERSHIP.md#agent-credentials)).
- Remote phone access is off by default. When turned on, the device gateway is
  served over HTTPS on the owner's tailnet only, devices pair with a one-time
  QR code and hold revocable, scoped grants ([REMOTE.md](REMOTE.md)).
- Provider credentials stay in each account profile's isolated home; they are
  never placed in URLs, profile values or logs
  ([SUBSCRIPTIONS.md](SUBSCRIPTIONS.md)).
