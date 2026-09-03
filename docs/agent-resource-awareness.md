# Shared machine resources (read before you compile or test)

Lazybox is a fleet tool: a dozen or more agents routinely run at once on the
same box — separate worktrees, focus-mode multi-workspace layouts, remote
sandboxes. Every one of them shares the same CPU, disk, and `target/`
directory. The failure mode is simple and common: fifteen agents each
*independently* decide to `cargo build` or `cargo test`, each spins up one
codegen job per core by default, and the machine pins at 100% CPU. Builds
crawl, the target dir thrashes, and interactive typing in the TUI stutters.

So before you kick off any heavy compile or test run, **check for available
resources first.** Do not blindly `cargo build` / `cargo test` — the box may
already be saturated by other agents doing exactly the same thing.

## Check the load before heavy work

Before `cargo build`, `cargo test`, `cargo clippy`, or any long compile,
sample the current system load and compare it against the core count:

```bash
nproc                 # cores available (macOS: sysctl -n hw.ncpu)
uptime                # load average — the three numbers after "load average:"
```

If the 1-minute load average is already at or above the core count, the
machine is saturated. Back off: wait for a slot, throttle the job count, or
narrow the scope before you add to the pile.

## Throttle the job count

Prefer bounding parallelism over grabbing every core — N agents each grabbing
every core *is* the failure mode. `CARGO_BUILD_JOBS` (or `--jobs`) is the lever
that actually reduces the load: it caps how many crates compile at once. `nice`
does something different and complementary — it only lowers scheduling
priority, so the build still uses the same job count and thrashes the same
`target/`, but the kernel lets interactive work jump ahead of it. Use both:

```bash
CARGO_BUILD_JOBS=4 cargo build        # or: cargo build --jobs 4 — caps the jobs
nice -n 10 cargo test -p <crate>      # only de-prioritizes; pair with -j to cut load
```

A build that uses half the cores and finishes is worth far more than one that
grabs them all and thrashes.

## Prefer scoped builds and tests

While you iterate, build and test only the crate you touched instead of the
whole workspace:

```bash
cargo test -p <crate>                 # tight edit/compile loop
cargo clippy -p <crate>
```

Scoped runs compile a fraction of the graph, so they finish faster and leave
headroom for everyone else.

**But scope narrowing is for the loop, not for the final green-before-push
check.** This workspace has cross-crate gates that a scoped run never compiles
and so silently passes: editing `crates/tui-core/src/action.rs` breaks
`crates/tui/tests/keymap_docs.rs`; touching `crates/ipc/src` bumps the desktop
protocol fingerprint; install-copy edits break `crates/tui/tests/web_docs.rs`;
`dep_rules.rs` polices layering. `cargo test -p <the-crate-you-edited>` is
green in every one of those cases while CI goes red. So before you push, run
the **full** gate suite (`cargo test --workspace`, `cargo clippy --workspace`,
`typos`, plus any `make` regen the change touches) — throttled with
`CARGO_BUILD_JOBS`/`nice` if the box is busy, but run, not skipped. The
resource rules above govern *how hard* you compile, never *whether* the final
validation happens.

## Coordinate, don't collide

Multiple agents compiling the same workspace at the same instant thrash both
the CPU and the shared `target/` directory. There is no shared build queue to
join — "wait for a slot" just means re-sample the load yourself: if `uptime`
shows the box already saturated, sleep and check again before you start,
rather than piling on. A build that starts 30 seconds later still beats two
builds fighting for the same cores.
