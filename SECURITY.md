# Security policy

## Supported versions

lazybox is pre-1.0. Security fixes are applied to the latest release and to
`main`; older releases are not maintained as separate security branches.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability involving
credentials, command execution, the HTTP API, worktree isolation, or terminal
parsing. Use GitHub's private security-advisory form:

<https://github.com/AntoineToussaint/lazybox/security/advisories/new>

Include the affected version or commit, platform, reproduction steps, expected
impact, and any suggested mitigation. Do not include real provider tokens or
private repository contents. You should receive an acknowledgement within
seven days; coordinated disclosure timing will be agreed before publication.

## Security boundaries

- Agent processes run with the same operating-system identity and repository
  access as the lazybox process. Lazybox is not a sandbox.
- The JSON HTTP API is plaintext HTTP and refuses non-loopback binds without a
  separate acknowledgement flag. Keep it on loopback or place it behind an
  authenticated TLS reverse proxy, SSH tunnel, or private overlay network.
- Provider credentials supplied through environment variables or configuration
  are available to the local process. API credential mutations are session-only
  until an encrypted persistent credential store is implemented.
