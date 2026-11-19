# Lazybox for iPhone (in progress)

The native iPhone app: an Expo (SDK 58, React Native 0.88) app in the V2 npm
workspaces. It pairs with a Runner like the phone web app does
([REMOTE.md](../../REMOTE.md)), and reaches it through the same device gateway,
with the same grants and the same allowlist. The direction and its spikes are in
[MOBILE_CLOUD_HUB.md](../../MOBILE_CLOUD_HUB.md#2-iphone-client).

**First vertical slice:** pair (scan the QR code or paste its link), the Inbox
(what needs you, across workspaces, with Allow, Deny and answers on the rows,
pull to refresh), a conversation (its latest turns, then live; the pending
request with Allow, Deny or the answer form), and the composer with **Send**,
**Queue** behind a running turn, **Send now** and **Stop**. Haptics on actions;
one connection state, said once.

The Inbox keeps completed conversations behind **Show completed**, with pinned
conversations and work that needs attention still visible. The composer uses
a compact layout by default. While a turn is active,
**Send now** sits above the draft and the send button queues the message.
Tap the microphone to dictate and tap again to finish. iOS asks for microphone
and speech recognition permission on first use. The recognized text is appended
to the editable draft. Tap Send or Queue when it is ready. iOS may process
speech through Apple's recognition service. The Runner receives the text when
you send it.

## How it is built

| Where | What |
| --- | --- |
| `app/` | Screens (expo-router): `index` (Inbox, or the welcome before pairing), `pair`, `conversation/[id]`, `settings`. |
| `src/models/` | View-models, pure and tested (`scripts/mobile-models.test.ts`): Inbox rows and sections, transcript rows, the request card and its answer, the composer, the connection state, Markdown blocks, the pairing deep link. |
| `src/runner/` | The connection: the pairing in the Keychain (`store.ts`), the P-256 device key (`p256.ts`), sign-in and credential renewal, `RunnerLink` over `expo/fetch`, polling only in the foreground (`RunnerProvider.tsx`), request actions with Undo (`actions.ts`). |
| `src/ui/` | Native components: theme (the computer's Dark and Light tokens), buttons, SF Symbols, rows, the transcript, the request card, the composer. |
| `e2e/` | Maestro flows run by `scripts/test-mobile-simulator.mjs`. |

What it shares, rather than reimplements:

- `@lazybox/client-core`: `RunnerLink`, `follow`, pairing and sign-in
  (`device.ts`, now with a pluggable `KeyProvider`).
- `@lazybox/surfaces/models`: the DOM-free half of the surfaces. Attention
  (what needs you), row plans, the request model, the transcript fold and
  `ConversationReader` (tail first, then live from the cursor), the phone's
  sections, the themes' tokens. `scripts/surfaces-models.test.ts` keeps that
  entry free of React, the DOM and CSS.
- `@lazybox/protocol`: the generated Runner service.

The app never imports a React DOM component. Why: MOBILE_CLOUD_HUB.md, spike S4.

## Run it on the iOS Simulator

You need Xcode with an iOS simulator runtime (`xcodebuild -downloadPlatform iOS`
if `xcrun simctl list runtimes` shows none), CocoaPods (`brew install cocoapods`)
and Node 24+. Everything below runs from the repository root; the simulator runs headless (no
Simulator.app window).

```bash
npm install                                   # the workspaces, Expo included
(cd apps/mobile && npx expo prebuild -p ios)  # writes apps/mobile/ios (not committed)
(cd apps/mobile/ios && pod install)

# A simulator, booted headless:
xcrun simctl list devices available | grep iPhone
xcrun simctl boot <udid>

# Build and install. Release carries its JavaScript; Debug loads it from Metro.
xcodebuild -workspace apps/mobile/ios/Lazybox.xcworkspace -scheme Lazybox \
  -configuration Release -sdk iphonesimulator -destination id=<udid> \
  -derivedDataPath /tmp/lazybox-mobile-build build
xcrun simctl install <udid> /tmp/lazybox-mobile-build/Build/Products/Release-iphonesimulator/Lazybox.app
xcrun simctl launch <udid> ai.lazybox.mobile
```

For development, build `-configuration Debug` instead and serve the JavaScript
with Metro: `CI=1 npm start -w @lazybox/mobile`. `CI=1` keeps Metro
non-interactive. Restart it to pick up changes, and never open its debugger
window from a script.

### Against the fixture Runner

`npm run mobile:fixture` starts the fake Runner of the UI tests with a phone
story in it: three requests waiting, a conversation an agent is writing in, a
failure, pins. It prints a pairing link. Hand it to the app:

```bash
xcrun simctl openurl <udid> "lazybox://pair?link=<the link, URL-encoded>"
```

Or copy it and use **Paste link**. The fixture plays the agent's side: an
allowed command runs, a reply is answered, and **Send now** stops the turn for
your message.

### Against a real Runner

The app talks to the device gateway the Runner serves for phones: Settings →
Devices → Access from your phone, then **Pair a phone**. On the simulator, scan
is not available (no camera): copy the pairing link and paste it. Only pair with a
Runner you mean to: a test should use an isolated one, never your daily Dev or
Working Runner.

### Tests

```bash
npm run typecheck                       # the workspace, then apps/mobile
npm run test:client                     # includes mobile-models and surfaces-models
MAESTRO=/path/to/maestro npm run test:mobile-simulator   # needs the Release app installed
```

`test:mobile-simulator` drives the app with [Maestro](https://maestro.dev)
(Java 17+). It pairs, opens the Inbox, allows a request, replies, and uses Send
now behind a running turn. It then checks what the fixture Runner received:
device credentials only, allowlisted RPCs only, the approval and the messages.
The lifecycle flow keeps a stream idle for three minutes, then sends the app
to the background and back. Screenshots, dark and light, go to
`.cache/mobile/`. Maestro installs as a release zip from GitHub if Homebrew
cannot install it.

## Run it on your iPhone

The phone must reach your Runner: today that is your tailnet (Tailscale on the
iPhone, signed in to the same tailnet; [REMOTE.md](../../REMOTE.md)). Pair by
scanning the QR code in the app.

**Without an Apple Developer account** (free provisioning):

1. In Xcode, open `apps/mobile/ios/Lazybox.xcworkspace`, select the Lazybox
   target → Signing & Capabilities, choose your Apple ID's **Personal Team**,
   and change the bundle identifier to one of your own (for example
   `com.<you>.lazybox`). Free teams cannot use `ai.lazybox.mobile`.
2. Turn on **Developer Mode** on the iPhone (Settings → Privacy & Security),
   connect it, choose it as the run destination, and choose the **Release**
   build configuration (Product → Scheme → Edit Scheme → Run), so the app
   carries its JavaScript and needs no Metro.
3. Run. The first time, trust the developer on the phone (Settings → General →
   VPN & Device Management).

A free profile **expires after 7 days**. The app then stops opening until you
build it again. A free team can have three such apps on a device. Push
notifications, TestFlight and App Store distribution are not available.

**With the Apple Developer Program** ($99 a year): a profile lasts a year,
the bundle id can be the project's, and builds can go to TestFlight. Push
needs it (APNs keys belong to a team).

## Next

Not in this slice, in rough order:

- **Push** through APNs with encrypted payloads and a Notification Service
  Extension, and **notification actions** (Allow, Deny, Reply). Needs the paid
  program and the push gateway (MOBILE_CLOUD_HUB.md §3.4).
- **The Secure Enclave key** (spike S2): the device key moves from the Keychain
  into the Secure Enclave through a small Swift module, as another `KeyProvider`.
- **Live Activities** for a running turn, **App Intents** (Shortcuts and Siri:
  "What needs me?"), **TestFlight**.
- The offline cache (last Inbox and tails in SQLite, read marks), swipe
  actions on rows, several Runners, spoken replies, diff review, PR status.
