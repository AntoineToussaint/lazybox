# Native GitHub OAuth login (no `gh` CLI)

Lazybox resolves a GitHub token through a credential chain
(`crates/gh-provider/src/lib.rs::credential_chain`). Historically the only
non-env source was `gh auth token`, so a machine without the `gh` CLI
installed and logged in could not authenticate. `lazybox auth login github`
adds a self-contained path using GitHub's [OAuth device flow].

## Chain order

```
LAZYBOX_GITHUB_TOKEN → GH_TOKEN → GITHUB_TOKEN → gh auth token → OAuth token
```

- An explicit env token still wins (unchanged for CI / scripted setups).
- `gh auth token` is used whenever `gh` is installed and authenticated, so
  nothing changes for existing users.
- The stored OAuth token is the **last resort** — it activates only when
  nothing above it resolves (the `gh`-not-installed case it exists for). It
  sits behind `gh` deliberately: a stored token that GitHub invalidates
  server-side (password reset, revoked authorization) still looks valid
  locally, so placing it ahead of `gh` would let a dead token shadow a
  working `gh` credential. Run `lazybox auth logout github` to clear a stored
  token that has stopped working.

## Usage

```bash
lazybox auth login github    # run the device flow, store the token
lazybox auth status          # show whether a token is stored + its scopes
lazybox auth logout github   # remove the stored token
```

`auth login` prints a short user code and a verification URL (and tries to
open it in a browser), then polls GitHub until you authorize. The resulting
token is written to `<LAZYBOX_HOME>/v2/oauth/github.json` (`0600` on unix).
Scopes requested: `repo read:org` — the same shape a default
`gh auth login` token carries, enough for reading and mutating PRs/issues
and enumerating org repositories.

## Client id

The device flow needs a registered GitHub **OAuth app** client id (public,
not a secret). It is resolved from:

1. `LAZYBOX_GITHUB_OAUTH_CLIENT_ID` (env override — for self-hosters
   pointing lazybox at their own OAuth app), then
2. the baked-in `BAKED_CLIENT_ID` constant in
   `crates/gh-provider/src/oauth.rs` (empty until lazybox's own OAuth app is
   registered).

If neither is set, `auth login` exits with a clear message rather than
attempting the flow.

## Ingress: local and remote

The device flow is **outbound only** — lazybox POSTs to `github.com` and
polls for the token. There is **no callback server and no inbound port**,
so it works identically for:

- a local daemon behind NAT,
- a remote/BYOR box (`lazybox serve`) — run `auth login` on the box over
  SSH; the printed URL can be opened from any browser,
- the desktop app.

This is the same NAT-friendly property that makes the device flow the right
first step before any webhook work.

## Not in scope here

- **Webhooks.** Real-time push (vs polling) needs a publicly reachable
  ingress and payload handling; the deliberate decision to keep polling for
  now, and the installation-token/relay design if that changes, live in
  [`github-app-webhook-decision.md`](github-app-webhook-decision.md).
- **Linear OAuth.** Linear uses an authorization-code flow that needs a
  redirect callback, unlike GitHub's device flow. `auth login linear`
  reports that it is not yet implemented; `LINEAR_API_KEY` / the `linear`
  CLI remain the paths for now.

[OAuth device flow]: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow
