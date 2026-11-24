.DEFAULT_GOAL := help
GO_BUILD_JOBS ?= 1
GOMAXPROCS ?= 1
export GOMAXPROCS
export GOFLAGS := -trimpath -p=$(GO_BUILD_JOBS)
GO_ROOT := services/runner/code
BENCH_STATE ?= $(abspath .cache/bench)
BENCH_CONFIG ?=
WORKTREE_ROOT ?= $(BENCH_STATE)/worktrees
ENDPOINT ?= $(BENCH_STATE)/endpoint.json
PROJECT ?= $(abspath .)
SESSION ?=
CARGO_BUILD_JOBS ?= 1
export CARGO_BUILD_JOBS
WORKSPACE_MANIFESTS := $(wildcard apps/*/package.json packages/*/package.json workers/*/package.json)
# Every unit test finishes within 30 seconds, and each suite enforces it per
# test (AGENTS.md; scripts/unit-test-timeouts.mjs fails the gate catalog
# otherwise). go test's -timeout bounds a whole package binary, so Go unit
# tests run through the guard, which stops a test that runs over and names it.
GO_TEST := node $(abspath scripts/go-test-guard.mjs)
# cargo test has no per-test deadline; nextest ends a test at 30 s. The crates
# are separate Cargo workspaces, so the one config is passed, not looked up.
NEXTEST := cargo nextest run --config-file $(abspath .config/nextest.toml) --locked

.PHONY: help deps generate check check-go check-workspaces build build-go build-worker build-web test test-real test-browser fmt fmt-check run web
help:
	@printf '%s\n' 'make install    Build Lazybox from this clone and install it (make update, make uninstall)' 'make dev        Build and run isolated Lazybox Dev' 'make stage      Validate and freeze a Working candidate' 'make promote RELEASE=<id>  Pin a candidate for the next Working launch' 'make working    Run the pinned Working installation' 'make run        Run the Codefly gRPC Runner with temporary ports' 'make desktop    Build and launch the native Tauri agent window' 'make web        Optional browser test client (prints its authenticated URL)' 'make check      Format, vet, typecheck, test, generated-code and workspace checks' 'make check-desktop Native Rust format, tests (the desktop crate and the snippet helper), and clippy' 'make test       Deterministic Go tests' 'make test-outlive  Race-checked tests that agents and commands outlive a Runner restart' 'make stress-go RUN=<test>  Repeat Go tests under GOMAXPROCS=1, -race and CPU load' 'make stress-ui SUITES=<names>  Repeat UI suites, several at once' 'make test-real  Explicit real-provider tests; requires existing provider login' 'make build      Compile the Runner, test CLI, SDK worker, and browser client' 'make browser    Install the Chromium the agent browser runs (BROWSER.md)'

# The agent browser's Chromium (BROWSER.md): Playwright's own headless build,
# in Playwright's cache, matched to the pinned playwright-core. Never the
# person's Chrome or its profile. Once installed it downloads nothing again:
# Playwright finds the build in its cache and returns in about a second.
.PHONY: browser
browser: deps
	npm run --silent install:browser

deps: node_modules/.package-lock.json
node_modules/.package-lock.json: package.json package-lock.json $(WORKSPACE_MANIFESTS)
	npm ci --ignore-scripts --no-audit --no-fund

generate: deps
	codefly sync service runner
	node_modules/.bin/buf generate

check: fmt-check check-go check-workspaces check-generated test test-environments
check-generated: deps
	python3 scripts/check-generated.py
	node_modules/.bin/buf lint
	node_modules/.bin/buf breaking --against contracts/runner-v1.binpb

check-go:
	cd $(GO_ROOT) && go vet ./...
# The browser service's tests (npm run test:browser-service) launch the agent
# browser's Chromium; `browser` installs it once and is a quick check after.
check-workspaces: deps browser
	npm run check:client
build: build-go build-worker build-web
build-go:
	mkdir -p bin
	cd $(GO_ROOT) && go build -o ../../../bin/lazybox-runner .
	cd $(GO_ROOT) && go build -o ../../../bin/lazybox ./cmd/lazybox
build-worker: deps
	npm run build:worker
build-web: deps
	npm run build:web

test: build-worker
	# The remote and terminal tests build their binaries once per package, in TestMain, only when they run.
	cd $(GO_ROOT) && LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js) LAZYBOX_V2_BROWSER_WORKER=$(abspath workers/browser/dist/main.js) $(GO_TEST) -timeout 6m ./...
test-real: build-worker
	cd $(GO_ROOT) && LAZYBOX_V2_REAL=1 LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js) go test -timeout 12m -v ./tests/integration -run TestReal

.PHONY: test-personality
test-personality: build-worker
	cd $(GO_ROOT) && LAZYBOX_V2_REAL=1 LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js) go test -timeout 12m -v ./tests/integration -run '^TestRealPersonality$$'

test-browser: deps
	LAZYBOX_V2_ENDPOINT='$(ENDPOINT)' npm run test:browser
fmt:
	gofmt -w $(GO_ROOT)/internal $(GO_ROOT)/cmd $(GO_ROOT)/work.go $(GO_ROOT)/tests
fmt-check:
	@test -z "$$(gofmt -l $(GO_ROOT)/internal $(GO_ROOT)/cmd $(GO_ROOT)/work.go $(GO_ROOT)/tests)"
run: build-worker
	codefly run service runner --runtime-context native --temporary-ports --headless --set 'runner:LAZYBOX_V2_STATE=$(BENCH_STATE)' --set 'runner:LAZYBOX_V2_WORKTREE_ROOT=$(abspath $(WORKTREE_ROOT))' --set 'runner:LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js)' --set 'runner:LAZYBOX_V2_BROWSER_WORKER=$(abspath workers/browser/dist/main.js)' --set 'runner:LAZYBOX_V2_WEB_DIR=$(abspath apps/web/dist)' $(if $(BENCH_CONFIG),--set 'runner:LAZYBOX_V2_CONFIG=$(abspath $(BENCH_CONFIG))')
web: deps
	LAZYBOX_V2_ENDPOINT='$(ENDPOINT)' node scripts/viewer.mjs

.PHONY: desktop build-desktop check-desktop
build-desktop: deps
	npm run build -w @lazybox/desktop
desktop: build-desktop
	LAZYBOX_V2_ENDPOINT='$(ENDPOINT)' LAZYBOX_V2_PROJECT='$(PROJECT)' LAZYBOX_V2_SESSION='$(SESSION)' node scripts/desktop.mjs
check-desktop: deps
	@cargo nextest --version >/dev/null 2>&1 || { echo 'cargo-nextest is missing: make install-check says how to install it (CONTRIBUTING.md).'; exit 1; }
	cargo fmt --manifest-path apps/desktop/src-tauri/Cargo.toml --check
	npm run build:ui -w @lazybox/desktop
	$(NEXTEST) --manifest-path apps/desktop/src-tauri/Cargo.toml
	$(NEXTEST) --manifest-path tools/lazybox-snippets/Cargo.toml
	cargo clippy --manifest-path apps/desktop/src-tauri/Cargo.toml --locked -- -D warnings

.PHONY: test-workspace native-workspace-check
test-workspace: deps
	npm run test:workspace
native-workspace-check: deps
	node scripts/native-workspace-check.mjs

.PHONY: test-settings
test-settings: deps
	npm run test:settings

.PHONY: test-dialogs
test-dialogs: deps
	npm run test:dialogs

.PHONY: test-empty-workspace
test-empty-workspace: deps
	npm run test:empty-workspace

.PHONY: test-attachments
test-attachments: deps
	npm run test:attachments

.PHONY: test-github
test-github: deps
	npm run test:github
	npm run test:github-token

.PHONY: dev working stage promote environments test-environments
dev:
	node scripts/environments.mjs dev
working:
	node scripts/environments.mjs working
stage:
	node scripts/environments.mjs stage
promote:
	node scripts/environments.mjs promote '$(RELEASE)'
environments:
	node scripts/environments.mjs status
.PHONY: share-subscriptions
share-subscriptions:
	node scripts/environments.mjs share-subscriptions '$(SUBSCRIPTION_STATE)'
test-environments:
	node --test --test-timeout=30000 scripts/go-test-guard.test.mjs scripts/managed-build.test.mjs scripts/environments.test.mjs scripts/runner-service.test.mjs scripts/runner-restart.test.mjs scripts/release-gates.test.mjs scripts/user-config.test.mjs scripts/release/release.test.mjs scripts/release/install.test.mjs scripts/release/publish-cask.test.mjs scripts/export-check.test.mjs scripts/check-private.test.mjs

# An installable release (RELEASING.md): dist/$(VERSION)/ with the DMG, the
# updater archive, latest.json, the cask, notes and checksums. Signs with the
# APPLE_* secrets in the environment, ad-hoc without them; publishes nothing.
# GATES=1 runs the release gate catalog first (CI runs it as separate jobs);
# LAUNCHD=1 adds the smoke test that installs a test launchd service.
.PHONY: release release-smoke release-notes
release: deps
	node scripts/release/build.mjs --version '$(VERSION)' $(if $(filter 1,$(GATES)),--gates) $(if $(filter 1,$(LAUNCHD)),--launchd) $(RELEASE_FLAGS)
# Install from a clone (docs/install.md#install-from-source): checks what the
# build needs and says how to get anything missing, builds the app from
# source (ad-hoc signed, no updater, no secrets), puts it in PREFIX
# (/Applications by default). On Linux, this installs the Runner runtime
# under PREFIX (default ~/.local/lib), with the CLI and web client.
# Both platforms set up the Runner service and link the
# lazybox command, as the app's first launch does. make update pulls and
# reinstalls; make uninstall keeps your data unless PURGE=1.
# PREFIX counts only on the command line: an exported PREFIX meant for
# another tool must not move the app. The build uses every core
# (INSTALL_JOBS), not the one-job default this Makefile keeps for shared
# build machines.
INSTALL_JOBS ?= $(shell getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)
INSTALL_ENV = PREFIX='$(if $(filter command line,$(origin PREFIX)),$(PREFIX))' LINK_DIR='$(LINK_DIR)' OPEN='$(OPEN)' SMOKE='$(SMOKE)' PURGE='$(PURGE)' CARGO_BUILD_JOBS='$(INSTALL_JOBS)' GOMAXPROCS='$(INSTALL_JOBS)' GOFLAGS='-trimpath -p=$(INSTALL_JOBS)'
NEED_NODE = command -v node >/dev/null 2>&1 || { echo 'Node.js is missing: install Node.js 24 or later (https://nodejs.org), then run make install again.'; exit 1; }
.PHONY: install uninstall update install-check
install:
	@$(NEED_NODE)
	@$(INSTALL_ENV) node scripts/release/install.mjs install
install-check:
	@$(NEED_NODE)
	@node scripts/release/install.mjs check
uninstall:
	@$(NEED_NODE)
	@$(INSTALL_ENV) node scripts/release/install.mjs uninstall
update:
	git pull --ff-only
	@$(MAKE) install

# The smoke tests against an already built app: RUNTIME=<Lazybox.app>/Contents/Resources/runtime
release-smoke:
	cd $(GO_ROOT) && LAZYBOX_V2_RELEASE_RUNTIME='$(abspath $(RUNTIME))' $(if $(filter 1,$(LAUNCHD)),LAZYBOX_V2_RELEASE_LAUNCHD=1) go test -count=1 -timeout 5m -v -run '^TestRelease' ./cmd/lazybox
# Regenerate the app's What's new from CHANGELOG.md.
release-notes:
	node scripts/release/changelog.mjs --write

.PHONY: runner-service-plan runner-service-install runner-service-start runner-service-stop runner-service-status runner-service-uninstall working-client
runner-service-plan runner-service-install runner-service-start runner-service-stop runner-service-status runner-service-uninstall:
	node scripts/runner-service.mjs $(patsubst runner-service-%,%,$@)
working-client:
	node scripts/runner-service.mjs client

.PHONY: test-repositories
test-repositories: deps
	npm run test:repositories
	npm run test:repository-picker

.PHONY: test-dogfood check-dogfood test-real-dogfood test-outlive test-real-outlive
test-dogfood: deps
	npm run test:dogfood
.PHONY: check-release list-release-gates
check-dogfood check-release: deps
	node scripts/release-gates.mjs run release
list-release-gates:
	node scripts/release-gates.mjs list release
test-real-dogfood: build-worker
	cd $(GO_ROOT) && LAZYBOX_V2_REAL=1 LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js) go test -timeout 6m -v ./tests/integration -run '^TestRealDogfoodContinuity$$'
OUTLIVE_PACKAGES := ./internal/sessionhost ./internal/redact ./internal/processguard ./internal/agent/... ./internal/runner ./internal/runner/commands ./internal/runner/engine ./internal/runner/rotation ./internal/runner/adapters/...
# Session hosts are re-executed test binaries, so they run with the race
# detector too; their reports would go to host.log, which a host deletes on
# exit. GORACE sends every process's reports to RACE_LOGS instead.
RACE_LOGS := $(abspath .cache/race)
test-outlive: build-worker
	rm -rf $(RACE_LOGS) && mkdir -p $(RACE_LOGS)
	cd $(GO_ROOT) && GORACE="log_path=$(RACE_LOGS)/race" LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js) $(GO_TEST) -race -count=1 -timeout 5m $(OUTLIVE_PACKAGES); \
	status=$$?; if ls $(RACE_LOGS)/race.* >/dev/null 2>&1; then cat $(RACE_LOGS)/race.*; echo 'data races reported (above)'; exit 1; fi; exit $$status
# The reboot gate (SESSION_HOSTS.md, "After a reboot"): the Runner, every session
# host and their programs are killed as a reboot kills them, a new Runner starts
# on the same state, and what must come back is checked. No credentials, no network.
.PHONY: test-reboot
test-reboot:
	cd $(GO_ROOT) && $(GO_TEST) -count=1 -timeout 1m -run '^(TestRebootBringsBackEverythingAndResumesTheAgent|TestAgentLostNoticeIsRecordedOnce)$$' ./internal/runner
test-real-outlive: build-worker
	cd $(GO_ROOT) && LAZYBOX_V2_REAL=1 LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js) go test -timeout 14m -v ./tests/integration -run '^TestRealAgentOutlivesRunnerRestart$$'

# Flake detection (RELEASE_GATES.md): repeat tests under the conditions that
# expose races. make stress-go RUN=TestName [PKGS=./internal/x] [COUNT=50] [LOAD=n]
# make stress-ui SUITES='chat-polish file-navigation' [RUNS=10] [PARALLEL=3] [LOAD=n]
.PHONY: stress-go stress-ui
stress-go: build-worker
	LAZYBOX_V2_WORKER=$(abspath workers/claude-sdk/dist/main.js) node scripts/stress.mjs go $(if $(COUNT),--count $(COUNT)) $(if $(LOAD),--load $(LOAD)) $(if $(RUN),--run '$(RUN)') $(PKGS)
stress-ui: deps
	node scripts/stress.mjs ui $(if $(RUNS),--runs $(RUNS)) $(if $(PARALLEL),--parallel $(PARALLEL)) $(if $(LOAD),--load $(LOAD)) $(SUITES)
