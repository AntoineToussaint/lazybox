---
name: add-a-provider
description: Add a new task source (GitHub, Linear, Jira, Slack are the existing ones) to lazybox — the crate, its credential chain, the poller wiring, filters, and setup detection. Use when adding or substantially reworking a provider, or when adding a new credential source for an existing one.
---

# Add a provider

A provider turns an external system into `Vec<Task>`. It must not know that a
TUI, a daemon or a store exists.

## 1. The crate

Create `crates/foo-provider/`, depending on `lazybox-core` and `lazybox-auth`
**only**. `crates/core/tests/dep_rules.rs` pins the internal dependency graph:
your new crate has to be added to the allowlist, and any edge beyond core and
auth fails the test. That failure is the design review — if you need a third
edge, justify it in the PR body rather than widening the list quietly.

Mirror an existing provider's shape: `client.rs` for the API surface, a
separate module for query building, and errors via `thiserror` (no `unwrap()`
in a library crate).

## 2. Credentials

Build a chain of `CredentialProvider`s rather than reading an env var
directly. GitHub's is
`EnvProvider("GH_TOKEN") → EnvProvider("GITHUB_TOKEN") → CommandProvider("gh auth token")`.

A new credential *source* (Vault, Keychain, OAuth) is just another
implementation: `name()` plus `async resolve(scope) → Credential`, added to
the chain in `crates/server/`.

Never copy a credential out of another component's config, and never hardcode
a token path — resolving credentials is what the chain is for.

## 3. Return real `Task`s

Map into `lazybox_core::Task`, and get the role right: a task whose payload
does not name the viewer is `TaskRole::Observer`, which the filters never
admit. Guessing a stronger role makes rows appear that the user never asked
for; guessing `Observer` makes real work vanish.

## 4. Wire it into the daemon

Add a source module under `crates/server/src/polling/sources/` alongside
`jira.rs`, register it in that module's `mod.rs` poller set, and follow the
same path the existing providers take through `spawn_handler.rs`. Provider
detection for the setup wizard lives in `crates/tui-boot/src/setup_detect.rs`
— a provider nobody can discover is a provider nobody enables.

Check what the default filter does to your tasks before concluding the poller
is broken: `ProviderConfig::default_for(...)` decides which kinds and roles
are admitted at all, and GitHub issues, for instance, are off by default.

## 5. Tests

Every public function needs one. Poller and filter behaviour belongs in
`crates/server/tests/polling.rs`-style tests against real shapes; do not mock
away the mapping you are trying to prove. Record what you could not exercise
(a live API, a credential you do not have) in the PR body.
