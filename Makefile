# lazybox — reactive PR inbox TUI
#
# Self-contained build: `make setup` downloads a pinned zig 0.16.0 to
# a host-level cache (`~/.cache/lazybox/zig/` by default) — the only
# out-of-band dependency. Caching it OUTSIDE the checkout means every
# clone and every git worktree shares one download instead of each
# re-fetching ~45MB into its own `vendor/zig/`. libghostty-rs is
# vendored under crates/libghostty-vt*. Build rules below prepend the
# pinned zig to PATH so any system zig is ignored. The pinned Ghostty source is
# prepared in the same shared cache instead of cloned under each Cargo OUT_DIR.
# Cross-platform:
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
GHOSTTY_CACHE ?= $(HOME)/.cache/lazybox/ghostty
CACHE_ZIG_DIR := $(ZIG_CACHE)/$(ZIG_SLUG)
# Resolve zig from either a per-worktree local install or the shared
# cache. A local vendor/zig wins when present (lets a worktree pin its
# own zig); otherwise use the cache `setup` populates.
LOCAL_ZIG_DIR := vendor/zig/$(ZIG_SLUG)
ZIG_DIR := $(if $(wildcard $(LOCAL_ZIG_DIR)/zig),$(LOCAL_ZIG_DIR),$(CACHE_ZIG_DIR))
PINNED_PATH := $(abspath $(ZIG_DIR)):$(PATH)

.PHONY: all setup build release run run-perf run-fresh run-test run-connect dev dev-fresh desktop desktop-deps desktop-preview desktop-build desktop-test desktop-contract web-control-contract contracts rebase-main test lint clean distclean install install-hooks help

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

setup: ## Prepare pinned Zig, Ghostty, and Cargo caches for offline builds (network used once).
	@command -v cargo >/dev/null || { echo "Error: cargo not found. Install Rust: https://rustup.rs"; exit 1; }
	@LAZYBOX_ZIG_CACHE="$(ZIG_CACHE)" LAZYBOX_GHOSTTY_CACHE="$(GHOSTTY_CACHE)" LAZYBOX_PREFETCH_BUILD=1 ./scripts/bootstrap.sh
	@command -v gh >/dev/null || echo "warning: gh not found — --test works, but GitHub-backed runs need the GitHub CLI or GH_TOKEN"

build: ## Build lazybox (debug). Uses pinned zig.
	@PATH="$(PINNED_PATH)" cargo build -p lazybox-tui-boot

release: ## Build lazybox optimized, strictly offline (run `make setup` once first).
	@PATH="$(PINNED_PATH)" LAZYBOX_GHOSTTY_CACHE="$(GHOSTTY_CACHE)" LAZYBOX_OFFLINE=1 CARGO_NET_OFFLINE=true cargo build --offline --locked -p lazybox-tui-boot --release

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
	@PATH="$(PINNED_PATH)" $(PERF_ENV) cargo run -p lazybox-tui-boot -- $(ARGS)

run-perf: ## Run with LAZYBOX_PERF=1 (writes /tmp/lazybox-perf.log; surfaces UI stalls + dropped keystrokes).
	@$(MAKE) run PERF=1 ARGS="$(ARGS)"

run-release: ## Same as `run` but optimized build. Use when debug feels sluggish (terminal scroll, large workspace lists). Build is ~10x slower but the binary is fast.
	@PATH="$(PINNED_PATH)" $(PERF_ENV) cargo run -p lazybox-tui-boot --release -- $(ARGS)

run-fresh: ## Run lazybox with --fresh (wipe state.db + force the setup wizard).
	@$(MAKE) run ARGS="--fresh"

run-test: ## Run lazybox with --test (tempdir + seeded session, no GitHub).
	@$(MAKE) run ARGS="--test"

run-connect: ## Connect to a running daemon socket. Usage: make run-connect SOCKET=/path
	@$(MAKE) run ARGS="--connect $(SOCKET)"

dev: ## Run the dev build against $(LAZYBOX_DEV_HOME) — independent state from `make run`.
	@echo "▶ dev profile: LAZYBOX_HOME=$(LAZYBOX_DEV_HOME)"
	@PATH="$(PINNED_PATH)" $(PERF_ENV) LAZYBOX_HOME="$(LAZYBOX_DEV_HOME)" cargo run -p lazybox-tui-boot -- $(ARGS)

dev-fresh: ## Same as `dev` but wipes the dev state.db first.
	@$(MAKE) dev ARGS="--fresh"

# ── desktop (the Tauri client under apps/desktop) ────────────────────
# The webview app speaks to the same daemon as the TUI over an
# authenticated loopback gateway. Needs Node 22 + npm on top of the
# usual toolchain; the Rust shell links libghostty-vt, so every recipe
# that compiles Rust carries the pinned-zig PATH like the TUI targets.
DESKTOP_DIR := apps/desktop
DESKTOP_MANIFEST := $(DESKTOP_DIR)/src-tauri/Cargo.toml

# `npm ci` only when the lockfile is newer than the install (or it's absent).
$(DESKTOP_DIR)/node_modules: $(DESKTOP_DIR)/package-lock.json
	@command -v npm >/dev/null || { echo "Error: npm not found. Install Node 22: https://nodejs.org"; exit 1; }
	@cd $(DESKTOP_DIR) && npm ci
	@touch $@

desktop-deps: $(DESKTOP_DIR)/node_modules ## Install the desktop npm deps (npm ci) when the lockfile moved.

desktop: desktop-deps ## Run the desktop app (Tauri dev) against its own in-process daemon.
	@cd $(DESKTOP_DIR) && PATH="$(PINNED_PATH)" npm run tauri dev

desktop-preview: desktop-deps ## Frontend only, on preview data — no daemon, no credentials.
	@echo "▶ open http://localhost:1420/?preview"
	@cd $(DESKTOP_DIR) && npm run dev

desktop-build: desktop-deps ## Build the debug macOS bundle → apps/desktop/src-tauri/target/debug/bundle/macos/lazybox.app
	@cd $(DESKTOP_DIR) && PATH="$(PINNED_PATH)" npm run tauri build -- --debug --bundles app

desktop-test: desktop-deps ## Headless desktop checks, as CI gates them (frontend tests + build + Rust shell tests).
	@cd $(DESKTOP_DIR) && npm test
	@cd $(DESKTOP_DIR) && npx playwright install chromium && npm run e2e
	@cd $(DESKTOP_DIR) && npm run build
	@PATH="$(PINNED_PATH)" cargo test --manifest-path $(DESKTOP_MANIFEST) --locked

desktop-contract: ## Regenerate apps/desktop/src/generated from the Rust desktop DTOs (CI fails on a diff).
	@PATH="$(PINNED_PATH)" cargo run -p lazybox-server --features desktop-contract --bin generate-desktop-contract
	@PATH="$(PINNED_PATH)" UPDATE_DESKTOP_CONTRACT=1 cargo test -p lazybox-server --test api_gateway desktop_compatibility_fixture_is_current -- --exact

web-control-contract: ## Regenerate crates/server/src/api_client_contract.json from the Rust web-control DTOs (CI fails on a diff).
	@PATH="$(PINNED_PATH)" UPDATE_WEB_CONTROL_CONTRACT=1 cargo test -p lazybox-server --test api_gateway web_control_contract_fixture_is_current -- --exact

contracts: desktop-contract web-control-contract ## Regenerate every generated wire contract (desktop + web-control).

rebase-main: ## Rebase the current branch onto origin/main, auto-regenerating the desktop contract on conflict.
	@PATH="$(PINNED_PATH)" ./scripts/rebase-onto-main.sh

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

install-hooks: ## Activate .githooks/ with the FULL gate (fmt + clippy + rustdoc + contract regen).
	@git config core.hooksPath .githooks
	@git config lazybox.precommitFull true
	@echo "Installed .githooks/ (full gate: fmt + clippy + rustdoc + contract regen)."
	@echo "Contracts regenerate only when a commit stages DTO sources (ipc/core/tui-core/api-gateway)."
	@echo "scripts/bootstrap.sh installs the fast fmt-only variant by default."
	@echo "Bypass any hook with \`git commit --no-verify\`."

clean: ## Clean cargo build artifacts (preserves the shared zig cache).
	@cargo clean

distclean: clean ## Clean cargo + the local and shared pinned-zig installs for this host.
	@rm -rf vendor   # per-worktree local install (and legacy layout)
	@rm -rf "$(CACHE_ZIG_DIR)"
	@rm -rf "$(GHOSTTY_CACHE)"

install: release ## Install to ~/.cargo/bin.
	@cp target/release/lazybox ~/.cargo/bin/lazybox
	@cp target/release/lb ~/.cargo/bin/lb
	@echo "Installed to ~/.cargo/bin/{lazybox,lb}"
