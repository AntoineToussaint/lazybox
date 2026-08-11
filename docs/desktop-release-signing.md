# Desktop release signing — operator runbook

`.github/workflows/release-desktop.yml` builds, signs, notarizes, and ships the
macOS desktop app (`.dmg` + Homebrew cask) on every `v<x.y.z>` tag. It is landed
and correct, but gated: every job is a no-op until an operator provisions six
`APPLE_*` secrets **and** flips the `DESKTOP_RELEASE_ENABLED` repository
variable to `true`. This is a one-time setup that requires an Apple Developer
account, so it cannot be automated — this runbook is the checklist.

Two footguns have tooling: `scripts/preflight-desktop-signing.sh` derives the
exact `APPLE_SIGNING_IDENTITY`/`APPLE_TEAM_ID` from your `.p12`, and audits
whether the repo is fully provisioned.

## 1. Obtain the certificate (Apple Developer account)

1. Enrol in the [Apple Developer Program](https://developer.apple.com/programs/)
   — this gives you the 10-character **Team ID**.
2. Create a **Developer ID Application** certificate
   (Certificates, IDs & Profiles → Certificates → +). This is the only cert
   type Gatekeeper accepts for a notarized app distributed **outside** the App
   Store — an "Apple Development" or "Mac App Distribution" cert will not work.
3. Export it from Keychain Access as a `.p12` (select the cert **and** its
   private key → Export → Personal Information Exchange). Set an export
   password; that password becomes `APPLE_CERTIFICATE_PASSWORD`.
4. Create an [app-specific password](https://support.apple.com/en-us/102654)
   for the Apple ID that owns the account. Notarization uses it — it becomes
   `APPLE_PASSWORD` (not your normal Apple ID password).

## 2. Derive the signing strings

`APPLE_SIGNING_IDENTITY` must match the certificate's Common Name character for
character or `tauri build` silently signs nothing. Read it off the `.p12`
rather than hand-typing it:

```console
$ bash scripts/preflight-desktop-signing.sh identity cert.p12
APPLE_SIGNING_IDENTITY=Developer ID Application: Your Name (ABCDE12345)
APPLE_TEAM_ID=ABCDE12345
```

(Pass the export password as a second argument if the `.p12` has one.)

## 3. Set the six secrets

Settings → Secrets and variables → Actions → Secrets, or via `gh`:

```console
$ base64 -i cert.p12 | gh secret set APPLE_CERTIFICATE
$ gh secret set APPLE_CERTIFICATE_PASSWORD    # the .p12 export password
$ gh secret set APPLE_SIGNING_IDENTITY        # from step 2
$ gh secret set APPLE_ID                       # the Apple account email
$ gh secret set APPLE_PASSWORD                 # the app-specific password
$ gh secret set APPLE_TEAM_ID                  # from step 2
```

`HOMEBREW_TAP_TOKEN` is already set and is reused to push the cask.

## 4. Dry-run before enabling

Set the variable, then validate signing with a `workflow_dispatch` run against
an existing tag — it builds, signs, and notarizes but skips the Release upload
and cask push, so nothing ships if it fails:

```console
$ gh variable set DESKTOP_RELEASE_ENABLED --body true
$ bash scripts/preflight-desktop-signing.sh check      # expect: ready
$ gh workflow run release-desktop.yml -f ref_tag=v0.1.7
```

The run's "Assert the bundle is notarized (fatal)" step (`stapler validate` +
`spctl --assess`) is the gate: it fails unless the app is genuinely notarized,
so a green dry-run means the credentials are good.

## 5. Ship and verify

Cut a real tag (the existing release flow). `release.yml` creates the GitHub
Release; `release-desktop.yml` attaches the `.dmg` and pushes
`Casks/lazybox-desktop.rb` to the tap. Then confirm a clean install:

```console
$ brew install --cask lazybox-desktop     # after `brew tap AntoineToussaint/lazybox`
```

The app should open with no Gatekeeper prompt — that is the end-to-end proof the
notarization stapled correctly.
