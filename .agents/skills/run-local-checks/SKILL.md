---
name: run-local-checks
description: Run lazybox's full local gate (fmt, clippy, rustdoc, nextest, and the crate-specific gates CI adds) before pushing or opening a PR, on a box shared with other agents. Use before any push, when a PR's CI is red and you want to reproduce it locally, or when deciding how hard to compile while the machine is busy.
---

# Run the local gate

`cargo build && cargo test` is **not** the gate. CI also runs fmt, clippy with
`-D warnings`, rustdoc with `-D warnings` (including private intra-doc links),
a loaded-profile test run, and a few crate-scoped jobs. Anything you skip
locally comes back as a red PR.

## First: check what the box is doing

This machine runs many agents at once. Sample the load before you compile:

```bash
uptime            # load averages
sysctl -n hw.ncpu # macOS core count   (Linux: nproc)
```

If load is already near the core count, back off rather than pile on:

- `CARGO_BUILD_JOBS=4 cargo …` to throttle.
- `cargo test -p <crate>` while iterating on one crate.
- Wait, if a full gate is what you need and the box is saturated.

Throttling changes *how hard* you compile, never *whether* the full gate runs
before you push — a scoped run misses cross-crate gates like the dependency
allowlist and the generated-docs drift tests. Full guidance:
[`docs/agent-resource-awareness.md`](../../../docs/agent-resource-awareness.md).

## The gate

```bash
make fmt            # format in place (do this first; fmt-check is the CI job)
make pre-commit     # fmt-check + clippy -D warnings + rustdoc -D warnings
make test           # cargo nextest, workspace, 10s per-test deadline
```

`make lint` alone is the workspace clippy config without the `--all-targets
-D warnings` that `pre-commit` and CI use, so prefer `pre-commit`.

Add, when your change touches them:

| Touched | Also run |
| --- | --- |
| `crates/ipc/src`, `crates/core/src`, `Cargo.lock` | the `regenerate-wire-contracts` skill |
| `apps/desktop/**` | `cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml --all-targets --locked` (separate workspace *and* separate lockfile) |
| the action catalog | `cargo test -p lazybox-tui --test keymap_docs` (generated reference drifts) |
| `web/**`, CLI commands, config keys | `cargo test -p lazybox-tui --test web_docs` |
| timing, PTYs, spawning | `make test-loaded` (CI runs the suite under two spinners per core) |

## Reading a failure honestly

Some failures here are the box, not your diff. Re-run the single test in
isolation before you "fix" it:

- Helper-spawn and stalled-clone tests fail on fixed timeouts under high load.
- macOS trust-store errors (`no native root CA certificates found`, Os -36)
  appear in bulk under load.
- `config_sandbox` races a pinned-home global.
- A bogus cargo compile error is sometimes a full disk — check `df -h` first,
  and clean only your own target directory.
- `typos` failing after minutes without naming a word means it could not
  download its own binary: `gh run rerun --failed`.

Isolation passing is evidence, not proof. If you could not reproduce a CI
failure locally, say so in the PR body rather than asserting it was flaky.

## Do not bypass

`git commit --no-verify`, a skipped test, or an `#[ignore]` added to make a
run green are not fixes. If a hook or gate is wrong, fix the hook — a gap in
the tooling is a bug in the tooling.
