# Contributing to Lazybox

Glad you're here. By participating you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md), and by opening a pull request you agree
to the [Contributor License Agreement](CLA.md). Only want to use Lazybox?
`make install` is all you need ([README](README.md#install-from-source)).

## Set up

On an Apple Silicon Mac, with what `make install-check` checks (Xcode Command
Line Tools, Go per `services/runner/code/go.mod`, Node.js 24+ with npm 11+,
Rust per `rust-toolchain.toml`), plus:

- the [Codefly](https://github.com/codefly-dev/cli) CLI, which builds and
  supervises the Runner in Dev and Working and generates its service code;
- `tmux`, for the session-host tests (`brew install tmux`);
- `cargo-nextest`, which `make check-desktop` runs the Rust tests under, at
  the version `.config/nextest.toml` requires or later
  (`cargo install --locked cargo-nextest@0.9.152`, or
  `brew install cargo-nextest`); `make install-check` reports it;
- network access on the first `make dev`: Dev and Working build the snippet
  helper (`tools/lazybox-snippets`), and Cargo fetches the library it links
  from the public [lazybox-v1](https://github.com/AntoineToussaint/lazybox-v1)
  at a pinned revision.

```sh
make deps            # locked npm dependencies (npm ci); again after package-lock.json changes
make dev             # build and run Lazybox Dev; it reloads as you edit
make help            # every target
```

**Dev, Working and the installed app** are three separate installations, each
with its own app identity, Runner, data and launchd job, so experimenting
never touches your real work ([ENVIRONMENTS.md](ENVIRONMENTS.md)):

| | What | How |
| --- | --- | --- |
| **Dev** | *Lazybox Dev*, running your checkout; the UI hot-reloads, Go changes restart the Runner | `make dev` (keep the terminal open) |
| **Working** | a frozen, checked build of your checkout, for day-to-day use | `make stage`, then `make promote RELEASE=<id>`, then `make working` |
| **Installed** | `Lazybox.app` in `/Applications` with its Runner as a launchd service, as testers get it | `make install` |

Agents outlive the Runner in all three: `Ctrl+C` in a launcher stops the
Runner, and the next one picks the agents up again.

## Test

Every behaviour change lands with a test; every bug fix with one that fails
without the fix. Every unit test finishes within 30 seconds, and the suites
stop one that does not and name it
([RELEASE_GATES.md](RELEASE_GATES.md#a-deadline-on-every-unit-test)): make a
slow test fast at its cause, never raise the limit.

```sh
npm run typecheck                 # TypeScript, every workspace
npm run test:client               # the client's pure models (node --test)
make test                         # the Runner's Go tests
make test-environments            # launcher, release and install scripts
npm run test:updates              # one UI suite (Playwright); npm run lists them all
make check                        # format, vet, typecheck, generated code, Go and client tests (installs the agent browser once)
make check-desktop                # the Tauri crate: fmt, tests (nextest), clippy
make check-release                # the full release catalog, every UI suite included
npm run docs:check                # build the docs site and check its links
```

UI suites drive the real interface against a fake Runner in headless
Chromium; install it once with `node_modules/.bin/playwright install chromium`.
Each one is registered in [`scripts/release-gates.json`](scripts/release-gates.json),
and a `test:` script that is not fails the catalog. Run a few by name with
`node scripts/release-gates.mjs run ui --only <ids>`;
[RELEASE_GATES.md](RELEASE_GATES.md) lists them and what CI runs.

## Branches, commits and pull requests

- One topic branch per piece of work, off `main` (`feat/…`, `fix/…`,
  `docs/…`), merged by pull request. Keep a branch to its topic.
- [Conventional Commits](https://www.conventionalcommits.org) for commit
  messages and pull request titles: `feat: …`, `fix: …`, `docs: …`,
  `chore: …`, `test: …`. Say what a person can now do, not which files moved.
- The pull request says what you verified and how, and what you could not
  (no provider login, no phone, a gate you skipped); its template asks for
  each, and whether each change is a fix or a hack ([AGENTS.md](AGENTS.md)).
- Generated code is never edited by hand: change
  `services/runner/proto/api.proto` and run `make generate`. Wire changes stay
  compatible with `contracts/runner-v1.binpb` unless [DECISIONS.md](DECISIONS.md)
  records otherwise.
- Docs are code: the pull request that changes behaviour updates the page in
  [`docs/`](docs/README.md) that describes it.
- Nothing from lazybox-v1 is reused without the review in
  [REUSE_AUDIT.md](REUSE_AUDIT.md).

## The changelog

[CHANGELOG.md](CHANGELOG.md) is what people read in the release notes and in
the app's **What's new**. A change someone would notice gets one line in the
**current version's section** (the `## [x.y.z] - date · title` at the top),
under its group: `- **What** what a person can now do`. Do not start a new
dated section above it; fold into the version being prepared. Then regenerate
the app's copy and commit both:

```sh
make release-notes      # node scripts/release/changelog.mjs --write
```

`make release` and `make install` refuse a changelog and app notes that
disagree.

## Where things live

```
services/runner/        the Runner: proto/api.proto, and the Go module in code/ (the lazybox CLI in code/cmd/lazybox)
packages/               protocol (generated), client-core, platform, surfaces (the shared React interface)
apps/desktop/           the Tauri app (its own Cargo workspace)
apps/web/               the browser client and the phone app
workers/claude-sdk/     the Claude Agent SDK worker; workers/browser the agent browser
scripts/                launchers, release and install, release gates, UI suites
scripts/release/        make release and make install (build.mjs, install.mjs)
packaging/              the release's Tauri overlay, disk image and cask templates
docs/                   the user documentation (also the docs site and the app's Help)
docs-site/              the docs site (VitePress)
tools/lazybox-snippets/ the snippet catalog helper (links V1's lazybox-config)
contracts/              the Buf compatibility baseline
audits/                 dated audits: history, not current guidance
```

The design notes at the top level (ARCHITECTURE.md, SESSION_HOSTS.md, …) are
indexed in [docs/internal/README.md](docs/internal/README.md);
[STRUCTURE.md](STRUCTURE.md) describes package boundaries. Coding agents follow
[AGENTS.md](AGENTS.md).

## Reporting bugs and requesting features

See [SUPPORT.md](SUPPORT.md). Report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md).
