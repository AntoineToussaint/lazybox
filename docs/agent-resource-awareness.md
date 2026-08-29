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
every core *is* the failure mode. Cap the codegen jobs and, on a busy box,
de-prioritize the process:

```bash
CARGO_BUILD_JOBS=4 cargo build        # or: cargo build --jobs 4
nice -n 10 cargo test -p <crate>      # yield to interactive work
```

A build that uses half the cores and finishes is worth far more than one that
grabs them all and thrashes.

## Prefer scoped builds and tests

Build and test only the crate you touched instead of the whole workspace:

```bash
cargo test -p <crate>                 # not: cargo test --workspace
cargo clippy -p <crate>
```

Scoped runs compile a fraction of the graph, so they finish faster and leave
headroom for everyone else. Reach for `--workspace` only when a change
genuinely spans crates.

## Coordinate, don't collide

Multiple agents compiling the same workspace at the same instant thrash both
the CPU and the shared `target/` directory. If a heavy build is already
running, wait for a slot rather than piling on — a queued build that starts
30 seconds later still beats two builds fighting for the same cores.
