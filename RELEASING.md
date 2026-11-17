# Releasing Lazybox

A release is an installable macOS app (Apple Silicon): `Lazybox.app` with the
Runner, the `lazybox` CLI, the Claude worker and a pinned Node inside, shipped
as a disk image and a Homebrew cask, and updating itself from a signed feed.
What people do with it is in [INSTALL.md](INSTALL.md).

| | |
| --- | --- |
| Tag | `v2-v<x.y.z>` (a namespace of its own: V1's `v0.1.21`-style tags and its cargo-dist workflows never see it) |
| Workflow | [`.github/workflows/release.yml`](.github/workflows/release.yml) |
| Local equivalent | `make release VERSION=x.y.z` → `dist/<x.y.z>/` beside the Makefile |
| Notes | [`CHANGELOG.md`](CHANGELOG.md), also the app's What's new |
| Update feed | `latest.json` on the rolling `v2-updates` GitHub release |
| Cask | `lazybox-v2` in `AntoineToussaint/homebrew-lazybox` |

## Cut a release

1. **Notes.** Add the section at the top of [CHANGELOG.md](CHANGELOG.md):
   `## [0.2.0] - 2026-12-01 · What this release is about`, one
   `- **What** what a person can now do` line per change. Then regenerate the
   app's copy and commit both:

   ```sh
   make release-notes              # writes packages/surfaces/src/release-notes.ts
   ```

   A build refuses a changelog and an app copy that disagree, and a version
   with no section.
2. **Optional dry run** on your Mac (nothing is published):

   ```sh
   make release VERSION=0.2.0                  # add GATES=1 to run the gate catalog first
   ```
3. **Tag and push** from the commit on `main`:

   ```sh
   git tag v2-v0.2.0 && git push origin v2-v0.2.0
   ```

The workflow then runs the gates (the same jobs as V2 CI), builds and signs on
macOS, smoke-tests the built Runner and CLI, and publishes: a GitHub release
for the tag (never marked *Latest*), `latest.json` on `v2-updates` (installed
apps pick it up within six hours), and the cask. A tag with a prerelease
suffix (`v2-v0.2.0-rc.1`) gets a prerelease and changes neither the feed nor
the cask. **Actions → V2 Release → Run workflow** with an existing tag
rebuilds it as a dry run: gates, build, signing, notarization, smoke test,
artifacts on the run, nothing published.

## What a build produces

`dist/<version>/`, beside the Makefile:

| File | What |
| --- | --- |
| `Lazybox_<v>_aarch64.dmg` | The disk image: drag to Applications (background, arrow, volume icon) |
| `Lazybox_<v>_aarch64.app.tar.gz` and `.sig` | The updater's archive and its minisign signature |
| `latest.json` | The feed entry: version, notes, date, archive URL and signature, plus the changelog's sections (below) |
| `lazybox-v2.rb` | The cask for this version (the workflow pushes it to the tap) |
| `RELEASE_NOTES.md` | The GitHub release body, from the changelog |
| `THIRD_PARTY_NOTICES.txt` | Every bundled component's licence (below); also in the app and on the disk image |
| `SHA256SUMS` | Checksums of everything above |
| `release.json` | How it was built: signing, notarization, updater key, Node, each runtime binary's signer |

The app bundle itself is at `.cache/release/target/release/bundle/macos/Lazybox.app`.
Its `Info.plist` has the release version as `CFBundleShortVersionString` and a
digits-only `CFBundleVersion` derived from it (`bundleVersion` in
`scripts/release/build.mjs`): macOS wants one to three integers that grow with
every build, and `0.1.0-pre.1` (or `0.1.0.1`, four of them) is not that. A
version `x.y.z-<label>.<n>` gets `x.y.(z×1000+n)` and `x.y.z` gets
`x.y.(z×1000+999)`, above all its prereleases: `0.1.0-pre.1` → `0.1.1`,
`0.1.0` → `0.1.999`, `0.1.1-rc.1` → `0.1.1001`. Number one version's
prereleases in one series (`pre.1`, `pre.2`, …, not `pre.2` then `rc.1`).

Inside the app, `Contents/Resources/runtime/`:

| Path | |
| --- | --- |
| `VERSION` | The release version; the CLI and Runner are built with the same one |
| `bin/lazybox`, `bin/lazybox-runner` | Go, `GOARCH=arm64`, `-trimpath`, version stamped (`internal/version`) |
| `node/bin/node` | Node pinned in [`scripts/release/node.json`](scripts/release/node.json), checksum-verified |
| `worker/` | The compiled Claude worker and exactly the production packages the lockfile resolved for it, including the Claude Code the SDK ships unless the build says `--without-claude-code` (below) |
| `web/` | The phone app the device gateway serves |

`Contents/Resources/THIRD_PARTY_NOTICES.txt` (and the same file on the disk
image, under the two icons) lists every component the app ships that Lazybox
did not write, with its licence, and prints each distinct licence text once
([`scripts/release/notices.mjs`](scripts/release/notices.mjs), run by every
build): the npm packages of the desktop app, the phone app and the worker,
the Go modules linked into the CLI and Runner (and Go itself), the Rust crates
the desktop app links, Node.js, and the Inter, JetBrains Mono and Newsreader
fonts with their SIL Open Font License. A component whose licence is not an
open-source one is listed at the top of the file, in the build's output and in
`release.json` (`notices.attention`); today that is the Claude Agent SDK and
the Claude Code binary it ships ("© Anthropic PBC. All rights reserved. Use is
subject to the Legal Agreements…") and the `codefly-dev` Go modules
(proprietary). Whether the release may redistribute them is the publisher's
decision, not the build's.

The worker's packages are resolved from each `package.json`'s dependencies
through `node_modules` on disk (`scripts/release/packages.mjs`), not from
`npm ls --parseable`, whose output hides a UUID-looking path segment (a
worktree, a CI checkout) as `***`.

### Built from source (`make install`)

`make install` (`scripts/release/install.mjs`) builds the same app with
`build.mjs --from-source`: ad-hoc signed whatever `APPLE_*` secrets are set,
**no updater** (no feed, no key, no updater archive, so the app never offers
an update it could not install), no disk image, cask or checksums, and a
`runtime/SOURCE.json` naming the commit and checkout, which Settings →
Updates and About show with *update with make update*. The version is the
newest one `CHANGELOG.md` names. It then copies `Lazybox.app` to `PREFIX`,
runs the bundled `lazybox service install` and `lazybox link` (what the
first launch does) and opens the app. Measured on an M-series Mac with four
build jobs: about 5 minutes from a fresh clone (Rust 3½ of them); the clone
then holds about 4.4 GB (`node_modules` 1 GB, `.cache/release` 3.4 GB) and the
app is 600 MB.

To qualify it without touching a real installation, run it with a scratch
`HOME` (and `CFFIXED_USER_HOME`), `LAZYBOX_SERVICE_LABEL=<a test label>`,
`SHELL=/bin/bash` (zsh completions go to Homebrew's shared
`site-functions`, outside `HOME`), `PREFIX=<scratch>`, `LINK_DIR=<scratch>`
and `OPEN=0`, keeping `RUSTUP_HOME`, `CARGO_HOME` and `GOPATH` pointed at the
real toolchains. The app keeps the release identity (`ai.lazybox.app`), whose
single-instance socket is shared with an installed copy, so look at its
window from a build whose `identifier` you changed.

### Why Node is bundled

The worker needs Node 24 or later. A launchd job starts with
`PATH=/usr/bin:/bin:/usr/sbin:/sbin`, so a Node installed with Homebrew, nvm
or mise is not found, and a person's Node version changes under the worker
whenever they switch projects. Requiring one would make "it starts at login"
depend on the shell setup of each machine. The bundled Node (about 110 MB,
the official build, keeping the Node.js Foundation's own Developer ID
signature) lives in its own directory, never on an agent's PATH: agents keep
the person's `node`. The same reasoning ships the Claude Code binary the
pinned SDK resolves (Anthropic's signature kept), so the worker and the CLI
it drives are the versions the gates ran.

### Building without Claude Code

Anthropic's terms say that preinstalling Claude Code in a product needs their
Commercial Terms, and its licence grants no redistribution
([CLAUDE_COMPLIANCE.md](CLAUDE_COMPLIANCE.md), R4). Whether a public release
ships it is the publisher's decision; the build can leave it out either way:

```sh
make release VERSION=0.2.0 RELEASE_FLAGS=--without-claude-code
# or: LAZYBOX_RELEASE_CLAUDE_CODE=omit make release VERSION=0.2.0
```

That build skips the SDK's per-platform packages (`@anthropic-ai/claude-agent-sdk-<platform>`)
in `worker/node_modules` and in the notices. The Runner then runs the `claude`
on its PATH (`claudeProgram`), Settings → Agents & workspaces → Claude says
where it found one, and **Install Claude Code** runs Anthropic's installer in
the shell panel when there is none.

Apple Silicon only for now. A universal build needs the x64 Node and SDK
packages next to the arm64 ones (`lipo` cannot merge them), doubling the
largest files, and an Intel test machine nobody has.

## Signing

Signing follows the secrets that exist. It is never a flag, and an unsigned
build always says so: in `release.json`, at the top of the release notes, and
in the release title (*Lazybox 0.2.0 (unsigned)*).

| Secrets present | Result |
| --- | --- |
| none | Ad-hoc signed. macOS asks people to confirm before the first launch ([INSTALL.md](INSTALL.md#unsigned-builds)). |
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` | Developer ID with the hardened runtime: the CLI and Runner (`codesign --options runtime --timestamp`), the app by Tauri, the disk image. |
| + `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | Also notarized and stapled: the app (by Tauri), then the disk image (`notarytool`, `stapler`). The build fails unless `stapler validate` and `spctl --assess` accept them. |
| or + `APPLE_API_KEY`, `APPLE_API_ISSUER`, `APPLE_API_KEY_P8` | The same, with an App Store Connect API key instead of an Apple ID (`APPLE_API_KEY_P8` is the `.p8` file, base64). |

The names are V1's ([docs/desktop-release-signing.md](https://github.com/AntoineToussaint/lazybox-v1/blob/28a9a73d1a44e7797d1285597257123a1db0a13f/docs/desktop-release-signing.md)),
so one set of repository secrets signs both. That runbook is how to get the
Developer ID certificate, derive `APPLE_SIGNING_IDENTITY`/`APPLE_TEAM_ID`
from the `.p12` (`scripts/preflight-desktop-signing.sh identity cert.p12`)
and create the app-specific password. Locally, `APPLE_APP_PASSWORD` is
accepted for `APPLE_PASSWORD`, and an identity in your login keychain needs
no `APPLE_CERTIFICATE`.

Node and Claude Code keep their publishers' Developer ID signatures; the
build checks each one is valid and has the hardened runtime, and records the
signer in `release.json`. No binary in the runtime needs an entitlement.

## The update feed

Installed apps read `latest.json` on the rolling `v2-updates` release
(`packaging/release.tauri.json` names it). It is the Tauri updater's format
plus one field of Lazybox's own:

```json
{
  "version": "0.2.0",
  "notes": "Title\n• What: body",
  "pub_date": "2026-12-01T10:00:00Z",
  "platforms": { "darwin-aarch64": { "signature": "<minisign signature of the .app.tar.gz>", "url": "https://github.com/…/v2-v0.2.0/Lazybox_0.2.0_aarch64.app.tar.gz" } },
  "lazybox": { "releases": [ { "id": "0.2.0", "date": "2026-12-01", "title": "…", "items": [ { "title": "…", "body": "…" } ] } ] }
}
```

- `lazybox.releases` is CHANGELOG.md from this version down (at most 25
  sections, `updaterReleases` in `scripts/release/changelog.mjs`). The app
  shows the ones newer than its own version in the install dialog, before
  anything is downloaded. The updater ignores the field; it decides nothing.
- `signature` is what decides: the app downloads the archive, verifies it
  against the public key it was built with, and installs nothing that does
  not match (the person sees *not signed with Lazybox's key*). `tauri
  build` writes the version into the signature's trusted comment, and the
  release configuration sets `requireSignedVersion`, so a feed cannot pair a
  new version number with an older signed archive (a downgrade).
- Nothing installs on its own. The app checks when it opens and every six
  hours (Settings → Updates → Check automatically, on by default, can be
  turned off), shows a quiet line in the header, and installs only on
  **Install and restart**. Skip this version and Remind me later are the
  person's to choose ([docs/install.md](docs/install.md#updates)).
- A check has a 20-second limit; a download has none (a few hundred
  megabytes on whatever network the person has). Failures reach the window
  as a kind (`offline`, `feed`, `signature`, `permission`, `install`;
  `updates::classify`), never as the updater's raw message alone.

### What an update does to running agents

`install_update` (`apps/desktop/src-tauri/src/updates.rs`):

1. Downloads and verifies the archive. Nothing has changed yet.
2. Replaces the app bundle: an atomic `renamex_np(RENAME_SWAP)` on APFS. The
   Runner, its session hosts and every agent keep running the executables
   they were started from (the old files stay alive until they exit). A
   session host never re-executes itself after it starts its program
   (`sessionhost/client.go`, `processguard`), and the Claude worker keeps
   one Claude Code process per conversation, so nothing mixes versions
   mid-turn.
3. Runs the new bundle's `lazybox service install`: `launchctl bootout` of
   the old Runner (SIGTERM; it detaches and leaves its hosts running, which
   live in sessions of their own) and `bootstrap` of the new one, which
   adopts every host (SESSION_HOSTS.md). If this step fails, the next launch
   does it (`release::prepare`).
4. Restarts the app, which reopens its windows.

The install dialog says this to the person, with the number of agents
running. A host speaking a wire protocol outside the new Runner's window
(`SESSION_HOSTS.md`, "Upgrades") cannot be adopted: it is stopped and its
conversation resumes natively. Keeping that window's lower bound at the
oldest supported release is what keeps updates agent-safe.

## Updater key

Every installed app verifies an update against the public key it was built
with. **The workflow refuses to build without the key**, since a release
signed with any other could never be installed as an update, and an app
built without a key (Dev, Working) has no updater at all
(`updates::configured`): signature verification cannot be turned off.

One-time setup:

```sh
node_modules/.bin/tauri signer generate -w ~/.tauri/lazybox-updater.key   # choose a password
cp ~/.tauri/lazybox-updater.key.pub packaging/updater.pub
git add packaging/updater.pub && git commit -m "chore(v2): the updater's public key"
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/lazybox-updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD                        # the password you chose
```

Keep the private key file somewhere safe as well (a password manager): losing
it means installed apps can no longer update themselves. Never commit it.

A local build without `TAURI_SIGNING_PRIVATE_KEY` generates a throwaway pair
in `.cache/release/updater-test.key` and says so; its app updates only
from builds signed with that throwaway key, which is what testing the updater
locally needs (`release.json` records `"updaterKey": "test"`).

### Rotate

- **Apple certificate or password**: replace the secrets. Already installed
  apps do not care which Developer ID signed an update (the updater checks
  its own key), and Gatekeeper accepts any valid Developer ID.
- **Updater key**, still in your hands: release once more signed with the
  old key but carrying the new public key (`packaging/updater.pub`) — build
  that release with the old `TAURI_SIGNING_PRIVATE_KEY` — then switch the
  secrets to the new key. Apps that skipped that release update with the old
  key until they reach it.
- **Updater key lost or leaked**: installed apps cannot be moved to a new key
  through the updater. Ship a release with the new key and ask people to
  reinstall it from the disk image or `brew reinstall --cask lazybox-v2`.
- **`HOMEBREW_TAP_TOKEN`**: a fine-grained token with contents write on the
  tap repository only; replace the secret. Without it the cask is attached to
  the release but not pushed.

## First release checklist

1. Generate the updater key and commit `packaging/updater.pub` (above).
2. Set `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
3. Optional, for a signed release: the `APPLE_*` secrets (above). Without
   them the first release ships unsigned and says so.
4. Check `HOMEBREW_TAP_TOKEN` can push to `AntoineToussaint/homebrew-lazybox`.
5. Make sure the repository the release lives in is **public**, or that the
   people installing can reach it: the disk image, the cask URL and the
   updater's `latest.json` are plain HTTPS downloads from GitHub releases,
   which a private repository does not serve to anyone signed out.
6. Date the release's section of the changelog (`0.1.0-pre.1` for the first
   prerelease: one curated section, its items under `### Group` headings),
   fold into it any dated sections merged above it since (the build refuses
   a version with dated sections above its own), `make release-notes`,
   commit.
7. Dry run: `make release VERSION=0.1.0 LAUNCHD=1` (the second smoke
   test installs a uniquely named test launchd job, updates it to a second
   copy of the build, checks a terminal survived, and removes it).
8. Tag `v2-v0.1.0` and push the tag.
9. **Publish the docs.** The site is `docs/` rendered by VitePress
   (`docs-site/`), the same Markdown the app's docs viewer shows. Preview it
   first: `npm run docs:check` (builds into `.cache/docs-site` and fails on a
   dead link or anchor), then `npm run docs:preview`; `npm run docs:dev`
   reloads as you edit. The standalone repository deploys it to GitHub Pages
   with its **Docs** workflow (`.github/workflows/docs.yml`).
   It runs only on a `v2-v*` tag and by hand, never on a push to `main`.
   - Once, before the first deployment: make the repository public (step 5),
     then Settings → Pages → Build and deployment → Source: **GitHub
     Actions**. The site is then served at
     `https://antoinetoussaint.github.io/lazybox/` (the workflow takes the
     base path from Pages, so a custom domain set there later needs no
     change here).
   - If Pages was enabled before step 8, the tag already deployed it: check
     the run under Actions → Docs. Otherwise, Actions → Docs → **Run
     workflow** on the tag.
   - Then link the site from the release notes and the repository's About
     box. Until Pages is enabled the workflow's deploy job fails and nothing
     is published; to never publish, leave Pages off or disable the workflow.
10. On a clean Mac: install from the disk image, check the first run, `lazybox
    doctor`, then `brew install --cask AntoineToussaint/lazybox/lazybox-v2`
    on another. Install the next release over it to see the update flow.

## Verification

| What | How |
| --- | --- |
| Plist, paths, install/update/uninstall, shell PATH | `go test ./internal/installation` (fake launchd; `plutil -lint` on the plist) |
| Standalone serve mode | `go test -run TestServe .` in `services/runner/code` |
| `service`, `doctor`, `link`, `uninstall --dry-run`, run/log resolution | `go test -run 'TestService\|TestDoctor\|TestLink\|TestRunAndLog' ./cmd/lazybox` |
| Built Runner serves, answers the CLI, stops on SIGTERM | `TestReleaseRunnerServesAndStops`, run by every `make release` |
| A service update keeps a terminal running | `TestReleaseServiceUpdateKeepsWhatRuns` (`LAUNCHD=1`) |
| Feed, cask, signing plan, release config, changelog | `node --test scripts/release/release.test.mjs` (in `make test-environments`) |
| Updater configuration, error kinds, feed notes, launch plan, CLI reports, feedback files | `cargo test` in `apps/desktop/src-tauri` (`updates::`, `release::`, `feedback::`) |
| Version order, skip and remind, automatic checks, running-agents warning, redaction | `npm run test:client` (`update-model.test.ts`, `feedback-model.test.ts`) |
| Settings → Updates and the install flow against a fake feed (offline, bad signature, skip, remind, install, post-update notice, version mismatch) | `npm run test:updates` |
| Help → Send feedback: preview equals what is saved, copied and put in the issue; redaction | `npm run test:feedback` |
| Resume after a reboot (Runner and every session host killed) | `make test-reboot` (SESSION_HOSTS.md, "After a reboot") |
| Workflows | `actionlint .github/workflows/release.yml .github/workflows/ci.yml .github/workflows/docs.yml` |

## What V2 reuses from V1

- **Secret names and the signing runbook**:
  [docs/desktop-release-signing.md](https://github.com/AntoineToussaint/lazybox-v1/blob/28a9a73d1a44e7797d1285597257123a1db0a13f/docs/desktop-release-signing.md),
  `scripts/preflight-desktop-signing.sh`, and V1's certificate import step
  (`apple-actions/import-codesign-certs`).
- **The workflow's shape** from `release-desktop.yml`: resolve the version
  from the tag, fail fast on missing secrets, build and sign on macOS, assert
  notarization with `stapler validate` and `spctl --assess`, upload the
  artifact, publish from Linux; a manual run against an existing tag is a dry
  run.
- **The tap and its publisher**: the cask goes to the same
  `AntoineToussaint/homebrew-lazybox` tap as V1's `lazybox` formula and
  `lazybox-desktop` cask, through V1's tested `scripts/publish-desktop-cask.sh`,
  copied with its regression test to `scripts/release/publish-cask.sh` so the
  standalone V2 repository carries it (its commit names the cask it publishes;
  `make test-environments` runs the test). The template follows
  V1's `lazybox-desktop.rb.tmpl` (`__VERSION__`/`__SHA256__`, `verified:`).
- **Release notes conventions**: a curated changelog, newest first, versions
  with dates, written for people using the product (V1's `CHANGELOG.md`),
  here also the source of the app's What's new.
- **Artifact naming and checksums**: `<Product>_<version>_<arch>.dmg` and a
  SHA-256 per artifact, as V1's DMG job writes.

In this repository, V1's release is unchanged except that `release.yml` and
`release-desktop.yml` now ignore `v2-v*` tags: their pattern
`**[0-9]+.[0-9]+.[0-9]+*` matches `v2-v0.1.0`, and cargo-dist would otherwise
have tried to release V1 under a V2 tag. The standalone repository
(`make export-repo`) gets this workflow as `.github/workflows/release.yml`.

## Not done

- Intel and universal builds (above).
- Notarization has not run: no Apple credentials were available while this
  was built. The ad-hoc path, the DMG, the updater artifacts and the smoke
  tests ran locally (`make release VERSION=0.0.0-test`).
- An update installed through the updater from one published release to the
  next has not run end to end: it needs two signed releases on a reachable
  feed. What that run must show: the header line appears without a
  download; the dialog lists the notes since the installed version and the
  running agents; Install and restart replaces the app, `lazybox doctor`
  then reports one version for app, CLI and Runner, and an agent that was
  mid-turn finishes its turn in the same process. Also check a feed whose
  archive is signed with another key is refused with the signature message.
- The repository is private: signed out, GitHub answers the feed with 404, so
  installed apps say *No update information is published* until it is public
  (First release checklist, step 5).
