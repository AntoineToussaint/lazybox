# lazybox — reactive PR inbox TUI
#
# Self-contained build: `make setup` downloads a pinned zig 0.15.2 to
# a host-level cache (`~/.cache/lazybox/zig/` by default) — the only
# out-of-band dependency. Caching it OUTSIDE the checkout means every
# clone and every git worktree shares one download instead of each
# re-fetching ~45MB into its own `vendor/zig/`. libghostty-rs is
# vendored under crates/libghostty-vt*. Build rules below prepend the
# pinned zig to PATH so any system zig is ignored. Cross-platform:
# detects host (macos/linux × arm64/x86_64) in scripts/bootstrap.sh.

# Detect host so PATH override picks the right vendored zig.
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)
ifeq ($(UNAME_S),Darwin)
  HOST_OS := macos
else ifeq ($(UNAME_S),Linux)
  HOST_OS := linux
else
  HOST_OS := unknown
endif
ifeq ($(UNAME_M),arm64)
  HOST_ARCH := aarch64
else ifeq ($(UNAME_M),aarch64)
  HOST_ARCH := aarch64
else ifeq ($(UNAME_M),x86_64)
  HOST_ARCH := x86_64
else
  HOST_ARCH := unknown
endif
# Single source of truth, shared with scripts/bootstrap.sh and CI.
ZIG_VERSION := $(shell cat .zig-version)
ZIG_SLUG := $(HOST_ARCH)-$(HOST_OS)-$(ZIG_VERSION)
# Pinned zig lives in a HOST-LEVEL cache, not inside the checkout, so
# every clone and worktree shares one download. Override the cache
# root with `LAZYBOX_ZIG_CACHE` (forwarded to bootstrap.sh by `setup`).
ZIG_CACHE ?= $(HOME)/.cache/lazybox/zig
CACHE_ZIG_DIR := $(ZIG_CACHE)/$(ZIG_SLUG)
# Resolve zig from either a per-worktree local install or the shared
# cache. A local vendor/zig wins when present (lets a worktree pin its
# own zig); otherwise use the cache `setup` populates.
LOCAL_ZIG_DIR := vendor/zig/$(ZIG_SLUG)
ZIG_DIR := $(if $(wildcard $(LOCAL_ZIG_DIR)/zig),$(LOCAL_ZIG_DIR),$(CACHE_ZIG_DIR))
PINNED_PATH := $(abspath $(ZIG_DIR)):$(PATH)

.PHONY: all setup build release run run-perf run-fresh run-test run-connect dev dev-fresh test lint clean distclean install help

# Side-by-side dev profile root. Picked up by `lazybox_core::paths`
# everywhere — independent state.db, worktrees, daemon socket, tmux
# socket, config. Override at the command line if needed:
#
#   make dev LAZYBOX_DEV_HOME=$HOME/.lazybox-experimental
LAZYBOX_DEV_HOME ?= $(HOME)/.lazybox-dev

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

all: setup build ## Setup dependencies and build

setup: ## Bootstrap: download pinned zig 0.15.2 to the shared cache (~/.cache/lazybox/zig).
	@LAZYBOX_ZIG_CACHE="$(ZIG_CACHE)" ./scripts/bootstrap.sh
	@command -v cargo >/dev/null || { echo "Error: cargo not found. Install Rust: https://rustup.rs"; exit 1; }
	@command -v gh    >/dev/null || { echo "Error: gh not found. Install: brew install gh (macOS) or https://cli.github.com"; exit 1; }

build: ## Build lazybox (debug). Uses pinned zig.
	@PATH="$(PINNED_PATH)" cargo build -p lazybox-tui

release: ## Build lazybox (optimized). Uses pinned zig.
	@PATH="$(PINNED_PATH)" cargo build -p lazybox-tui --release

# `make run` accepts args via ARGS=... (`make run ARGS="--fresh"`).
# Convenience targets below shorten the common cases.
ARGS ?=

# Set PERF=1 to enable the opt-in perf observability (run-loop watchdog,
# dropped-keystroke counter, on-screen stall indicator) and a dedicated
# perf log at /tmp/lazybox-perf.log. Off by default — see
# crates/tui/src/perf.rs. Use `make run PERF=1` or the `run-perf` target.
PERF ?=
PERF_ENV := $(if $(filter 1,$(PERF)),LAZYBOX_PERF=1,)

run: ## Build and run lazybox. Pass extra args via ARGS=, perf via PERF=1.
	@PATH="$(PINNED_PATH)" $(PERF_ENV) cargo run -p lazybox-tui -- $(ARGS)

run-perf: ## Run with LAZYBOX_PERF=1 (writes /tmp/lazybox-perf.log; surfaces UI stalls + dropped keystrokes).
	@$(MAKE) run PERF=1 ARGS="$(ARGS)"

run-release: ## Same as `run` but optimized build. Use when debug feels sluggish (terminal scroll, large workspace lists). Build is ~10x slower but the binary is fast.
	@PATH="$(PINNED_PATH)" $(PERF_ENV) cargo run -p lazybox-tui --release -- $(ARGS)

run-fresh: ## Run lazybox with --fresh (wipe state.db + force the setup wizard).
	@$(MAKE) run ARGS="--fresh"

run-test: ## Run lazybox with --test (tempdir + seeded session, no GitHub).
	@$(MAKE) run ARGS="--test"

run-connect: ## Connect to a running daemon socket. Usage: make run-connect SOCKET=/path
	@$(MAKE) run ARGS="--connect $(SOCKET)"

dev: ## Run the dev build against $(LAZYBOX_DEV_HOME) — independent state from `make run`.
	@echo "▶ dev profile: LAZYBOX_HOME=$(LAZYBOX_DEV_HOME)"
	@PATH="$(PINNED_PATH)" $(PERF_ENV) LAZYBOX_HOME="$(LAZYBOX_DEV_HOME)" cargo run -p lazybox-tui -- $(ARGS)

dev-fresh: ## Same as `dev` but wipes the dev state.db first.
	@$(MAKE) dev ARGS="--fresh"

test: ## Run all tests (cargo-nextest enforces a 10s per-test deadline).
	@PATH="$(PINNED_PATH)" cargo nextest run --workspace

test-ignored: ## Run #[ignore]'d real-backend integration tests on demand.
	@PATH="$(PINNED_PATH)" cargo nextest run --workspace --run-ignored only

lint: ## Run clippy with workspace lint config (vendored crates excluded).
	@PATH="$(PINNED_PATH)" cargo clippy --workspace --tests

fmt: ## Format every crate in-place. Run before committing.
	@cargo fmt --all

fmt-check: ## Verify formatting WITHOUT modifying files. Matches CI's fmt job.
	@cargo fmt --all -- --check

pre-commit: fmt-check ## Run the full gate by hand (fmt + clippy + rustdoc).
	@PATH="$(PINNED_PATH)" cargo clippy --workspace --all-targets -- -D warnings
	@PATH="$(PINNED_PATH)" RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --quiet

install-hooks: ## Activate .githooks/ with the FULL gate (fmt + clippy + rustdoc).
	@git config core.hooksPath .githooks
	@git config lazybox.precommitFull true
	@echo "Installed .githooks/ (full gate: fmt + clippy + rustdoc)."
	@echo "scripts/bootstrap.sh installs the fast fmt-only variant by default."
	@echo "Bypass any hook with \`git commit --no-verify\`."

clean: ## Clean cargo build artifacts (preserves the shared zig cache).
	@cargo clean

distclean: clean ## Clean cargo + the local and shared pinned-zig installs for this host.
	@rm -rf vendor   # per-worktree local install (and legacy layout)
	@rm -rf "$(CACHE_ZIG_DIR)"

install: release ## Install to ~/.cargo/bin.
	@cp target/release/lazybox ~/.cargo/bin/lazybox
	@cp target/release/lb ~/.cargo/bin/lb
	@echo "Installed to ~/.cargo/bin/{lazybox,lb}"
