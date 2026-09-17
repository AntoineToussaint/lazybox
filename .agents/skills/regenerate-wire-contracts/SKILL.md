---
name: regenerate-wire-contracts
description: Regenerate lazybox's generated wire contracts (the desktop TypeScript fixtures and the web-control JSON fixture) after changing IPC or core DTOs, and resolve the fingerprint conflicts they cause during a rebase. Use when adding or changing a Command/Event variant, when CI reports a stale desktop contract, or when a rebase conflicts inside a generated file.
---

# Regenerate the wire contracts

Two fixtures are generated from Rust DTOs and checked in:
`apps/desktop/src/generated` (desktop) and
`crates/server/src/api_client_contract.json` (web control).

```bash
make contracts          # both
make desktop-contract   # desktop only
```

The generator build is slow (~11 minutes cold), which is why the pre-commit
hook guards it with `scripts/regen-contracts.sh --if-staged` and runs it only
when a DTO source is staged.

## What moves a fixture

`scripts/regen-contracts.sh` lists the inputs: `crates/ipc/src/`,
`crates/core/src/`, `crates/tui-core/src/`,
`crates/server/src/api_gateway.rs`, and `Cargo.lock`. Two of those are easy to
miss:

- **`Cargo.lock` alone** moves the desktop compatibility fixture, because the
  protocol fingerprint hashed by `crates/ipc/build.rs` covers it. A bare
  `cargo update` can make the fixture stale with no source edit.
- A change under `crates/tui-core/src/` reaches the fixtures through the ts-rs
  exports even though nothing about it looks like wire code.

Editing a `.md` file under these crates does not — the fingerprint hashes
source, not docs.

## Adding a Command or Event variant

Adding a variant to `crates/ipc/src/protocol.rs` takes three coordinated
edits: the variant itself, a sample in the corpus, and the count assertion.
The count conflicts on essentially every rebase and has **no correct side** —
resolve it by taking both variants and recounting, never by picking one.

Removing or changing a field cannot desync bincode peers, because the
handshake is fingerprint-guarded: regenerate the contract rather than bumping
`DESKTOP_PROTOCOL_VERSION`, which means something else.

## Conflicts during a rebase

A conflict *inside* a generated file has no correct side either — both sides
are output. Do not hand-merge it:

```bash
make rebase-main    # rebases onto origin/main, regenerating on conflict
```

If you are mid-rebase already, regenerate and continue:

```bash
make contracts && git add apps/desktop/src/generated crates/server/src/api_client_contract.json
git rebase --continue
```

## Verifying

CI's desktop-contract job is currently disabled (the desktop shell is not a
release blocker), so a stale fixture will **not** always fail the PR. That is
a reason to regenerate deliberately, not a reason to skip it — the
`desktop-check` job still gates the shell compiling:

```bash
cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml --all-targets --locked
```

Note the desktop app is a separate workspace with its own lockfile, so a
root-only dependency bump does not touch it.
