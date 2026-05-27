# pilot — reactive PR inbox TUI
#
# Self-contained build: `make setup` downloads a pinned zig 0.15.2
# to vendor/zig/ — that's the only out-of-band dependency, and it
# lives inside the repo afterward. libghostty-rs is vendored under
# crates/libghostty-vt*. Build rules below prepend the pinned zig
# to PATH so any system zig is ignored. Cross-platform: detects
# host (macos/linux × arm64/x86_64) in scripts/bootstrap.sh.

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
ZIG_VERSION := 0.15.2
ZIG_DIR := vendor/zig/$(HOST_ARCH)-$(HOST_OS)-$(ZIG_VERSION)
PINNED_PATH := $(abspath $(ZIG_DIR)):$(PATH)

.PHONY: all setup build release run run-fresh run-test run-connect dev dev-fresh test lint clean distclean install help

# Side-by-side dev profile root. Picked up by `pilot_core::paths`
# everywhere — independent state.db, worktrees, daemon socket, tmux
# socket, config. Override at the command line if needed:
#
#   make dev PILOT_DEV_HOME=$HOME/.pilot-experimental
PILOT_DEV_HOME ?= $(HOME)/.pilot-dev

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

all: setup build ## Setup dependencies and build

setup: ## Bootstrap: download pinned zig 0.15.2 to vendor/zig/.
	@./scripts/bootstrap.sh
	@command -v cargo >/dev/null || { echo "Error: cargo not found. Install Rust: https://rustup.rs"; exit 1; }
	@command -v gh    >/dev/null || { echo "Error: gh not found. Install: brew install gh (macOS) or https://cli.github.com"; exit 1; }

build: ## Build pilot (debug). Uses pinned zig.
	@PATH="$(PINNED_PATH)" cargo build -p pilot-tui

release: ## Build pilot (optimized). Uses pinned zig.
	@PATH="$(PINNED_PATH)" cargo build -p pilot-tui --release

# `make run` accepts args via ARGS=... (`make run ARGS="--fresh"`).
# Convenience targets below shorten the common cases.
ARGS ?=

run: ## Build and run pilot. Pass extra args via ARGS=, e.g. ARGS="--fresh".
	@PATH="$(PINNED_PATH)" cargo run -p pilot-tui -- $(ARGS)

run-release: ## Same as `run` but optimized build. Use when debug feels sluggish (terminal scroll, large workspace lists). Build is ~10x slower but the binary is fast.
	@PATH="$(PINNED_PATH)" cargo run -p pilot-tui --release -- $(ARGS)

run-fresh: ## Run pilot with --fresh (wipe state.db + force the setup wizard).
	@$(MAKE) run ARGS="--fresh"

run-test: ## Run pilot with --test (tempdir + seeded session, no GitHub).
	@$(MAKE) run ARGS="--test"

run-connect: ## Connect to a running daemon socket. Usage: make run-connect SOCKET=/path
	@$(MAKE) run ARGS="--connect $(SOCKET)"

dev: ## Run the dev build against $(PILOT_DEV_HOME) — independent state from `make run`.
	@echo "▶ dev profile: PILOT_HOME=$(PILOT_DEV_HOME)"
	@PATH="$(PINNED_PATH)" PILOT_HOME="$(PILOT_DEV_HOME)" cargo run -p pilot-tui -- $(ARGS)

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

pre-commit: fmt-check ## Same gate the .githooks/pre-commit hook runs (fmt + clippy).
	@PATH="$(PINNED_PATH)" cargo clippy --workspace --all-targets -- -D warnings

install-hooks: ## Activate the in-tree .githooks/* (one-time per clone).
	@git config core.hooksPath .githooks
	@echo "Installed .githooks/. Bypass any hook with \`git commit --no-verify\`."

clean: ## Clean cargo build artifacts (preserves vendor/).
	@cargo clean

distclean: clean ## Clean cargo + vendored dependencies.
	@rm -rf vendor

install: release ## Install to ~/.cargo/bin.
	@cp target/release/pilot ~/.cargo/bin/pilot
	@echo "Installed to ~/.cargo/bin/pilot"
