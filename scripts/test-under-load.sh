#!/usr/bin/env bash
# Run the workspace test suite under deliberate CPU oversubscription (#1751).
#
# The box this suite normally runs on carries a fleet of agents compiling at
# once, so a test that only passes on an idle machine is not passing. This
# script reproduces that condition on demand — locally and in the merge
# queue's `loaded` lane: it compiles the suite unloaded, then pins
# `--load-factor` busy-loop spinners per core for the run itself, so the
# load lands on the tests and not on the compile.
#
#   scripts/test-under-load.sh                  # one run, 2 spinners per core
#   scripts/test-under-load.sh --runs 10        # the #1751 acceptance bar
#   scripts/test-under-load.sh --load-factor 0  # repeat runs, no spinners
#   scripts/test-under-load.sh --profile ci -- -p lazybox-server
#
# Anything after `--` is passed to both `cargo nextest` invocations.
# The spinners run at normal priority on purpose: `nice` would let the tests
# win the scheduler, which is exactly the headroom a loaded box does not give.
# On a shared dev box, prefer `--load-factor 0` while others are working.
set -euo pipefail

runs=1
factor=2
profile=default

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --runs) runs=$2; shift 2 ;;
        --load-factor) factor=$2; shift 2 ;;
        --profile) profile=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --) shift; break ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

cores=$(nproc 2>/dev/null || sysctl -n hw.ncpu)
spinners=$((cores * factor))

cargo nextest run --workspace --profile "$profile" --no-run "$@"

pids=()
cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for ((i = 0; i < spinners; i++)); do
    ( while :; do :; done ) &
    pids+=($!)
done
echo "test-under-load: $spinners spinners on $cores cores (load factor $factor), $runs run(s)"

for ((run = 1; run <= runs; run++)); do
    echo "=== run $run/$runs · $(uptime | sed 's/.*load/load/')"
    if ! cargo nextest run --workspace --profile "$profile" "$@"; then
        echo "test-under-load: run $run/$runs FAILED" >&2
        exit 1
    fi
done
echo "test-under-load: all $runs run(s) green"
