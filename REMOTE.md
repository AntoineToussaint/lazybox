# Remote control from a phone

Status: first increment implemented 2026-11-16; the phone that acts, with web
push, 2026-11-17 (audit brief R7p). A phone paired by QR code follows and
answers the agents on a local Runner from anywhere on the owner's tailnet:
from the Inbox row itself, from a notification, or in the conversation. The
native iPhone app and an off-tailnet relay are deferred (below). This is a Runner-local capability; it does not replace the
Hub's device identity in [device experience](DEVICE_EXPERIENCE.md).

## What the person does

1. **Settings → Devices → Access from your phone → Turn on.** The Runner asks
   Tailscale to serve its device gateway on this machine's tailnet name over
   HTTPS (`https://<machine>.<tailnet>.ts.net:8443`). The card says exactly
   what is missing when it cannot (Tailscale not installed, not signed in,
   HTTPS certificates off, port taken, Funnel on).
2. **Pair a phone.** A QR code appears at once, with the usual grants
   ("See conversations · Reply, answer and approve"); **Change** lets the
   owner drop replying, which issues a new code. The `start` grant is not
   offered (`offered` in `DevicesSettings.tsx`) until the phone has a screen
   to start a conversation from; a phone paired with it keeps it.
   The code works once and for five minutes; the dialog counts down, follows
   it, and closing the dialog withdraws it.
3. **On the phone**, the camera opens the link. The page lists what the phone
   will be able to do, proposes a name ("iPhone"), and **Pair this phone**
   creates the phone's key. Both screens then show the same key fingerprint.
4. **The phone's inbox** groups every conversation by what it needs — needs
   input, failed, ready for review, working, done — across workspaces, with
   the pins shared with the computer (a pin never hides a request). A
   conversation shows its transcript, its pending approval or question with
   Allow/Decline or an answer form, and a composer above the keyboard. Back
   (button or gesture) returns to the inbox. Interrupt is there while a turn runs.
5. **Inbox rows act** (R7p). An approval row shows the command and its flags
   with **Deny** and **Allow**; a question with up to six short options shows
   them as chips (one tap, or several then **Send**). An answer waits 4.5 s for
   **Undo**, then goes; leaving the page sends it at once. A flagged command
   (the computer's risk rules: deletes recursively, discards work, network,
   root, publishes, outside the workspace) or a provider's "default to no"
   asks first in a sheet whose focus is on Cancel — on the row and on the
   conversation's own card alike. Deny takes an optional reason. A diff, a
   settings change, several questions or a typed answer open the
   conversation instead. **Swipes:** right allows an approval row, or pins any
   other; left reveals Read, Pin and Stop (a long swipe marks read); Stop
   confirms. A long press or **⋯** opens every action. Pull down refreshes;
   offline and "can't reach your computer" are said in a bar with Retry.
6. **Notifications.** The Inbox offers **Turn on notifications** once (and
   Settings on the phone always). The phone asks permission inside the tap,
   subscribes with the Runner's key and hands the subscription to the Runner.
   The phone chooses the kinds — needs input, failed, ready for review, the
   same three and defaults as the computer's Settings → Notifications — quiet
   hours in its own time zone, and can send itself a test. A tap on a
   notification opens its conversation, ready to answer. On an iPhone this
   needs Lazybox on the Home Screen (iOS 16.4+); in a Safari tab the phone says
   how. Android Chrome needs nothing more.
7. **A command's output.** A conversation lists its commands under the header,
   live ones first; one opens the computer's console view (search, folds, text
   size), following new output. A phone reads output; it cannot stop or rerun.
8. **Settings → Devices** lists each paired phone with its grants, fingerprint,
   paired date and last seen ("Active now"), whether its notifications are on
   and for what, a **Send test**, and **Revoke**, which signs the phone out at
   its next request and drops its subscription; the phone then says so and
   forgets its key.

## Threat model

Assets: the owner's bearer (can do anything), provider and GitHub
credentials, the ability to approve tool calls (code execution on the
machine), conversation content.

| Adversary | What they could try | What stops it |
| --- | --- | --- |
| Anyone on the same Wi-Fi / LAN | Reach the Runner's ports | Every Runner listener, including the device gateway, binds loopback only and refuses to start otherwise. Nothing listens on the LAN. |
| The internet | Reach the gateway | The route is `tailscale serve` (tailnet only), never Funnel. The Runner refuses to serve a port where Funnel is on, and the gateway rejects any request Tailscale marks as having come through Funnel (`Tailscale-Funnel-Request`). |
| Another device on the owner's tailnet (or a shared tailnet) | Call the gateway | It needs a device credential. The gateway's RPC endpoint accepts device credentials only; pairing needs an unexpired, unused one-time code that only the owner's screen shows. |
| Someone who photographs the QR code | Pair their own phone | The code works once and for 5 minutes; the owner's dialog shows which device paired, with its name and key fingerprint, and Revoke is one click. Closing the dialog withdraws an unused code. (Residual risk: a photograph used within that window before the owner's phone.) |
| Brute-forcing a code | Guess the secret | 256-bit secret; five wrong attempts burn the offer; each offer answers at most 30 pairing requests a minute. The budget is per offer, so a peer without the code cannot spend it and block the person's pairing. |
| Another device on the tailnet | Keep a phone from signing in | Sign-in challenges are bounded per device (8 pending), so flooding one device's challenges blocks no other device, and needs that device's id. |
| A web page in a browser on this computer (DNS rebinding) | Reach the loopback gateway under its own name | The gateway answers only a `Host` that is loopback or this machine's tailnet name (`tailscale serve` keeps the phone's `Host`); any other gets `421`. A name it does not know makes it ask Tailscale again at most every 30 s, so a gateway that started before Tailscale connected is still reached. |
| A stolen access credential (logs, a proxy) | Replay it | It lasts 15 minutes and stops at once on revoke, streams it opened included. Credentials are bearer headers, never cookies, so there is no CSRF; no CORS headers are sent. |
| A copy of the phone's storage | Become the phone | The phone's ECDSA P-256 private key is a non-extractable WebCrypto key: script can sign with it but cannot read it. A new credential needs a fresh single-use challenge signed with it. |
| A paired phone (lost, or a mistaken grant) | Escalate | The device allowlist (below) is exhaustive and pinned by tests; settings, subscriptions, GitHub, terminals, commands, files, deletion, processes, restart and device management are unreachable. Changing an agent's permission mode (e.g. to yolo) is refused even with every grant. Info hides account usage, emails, sign-in flows and gateways. |
| An agent on this machine | Use the phone's authority | Agent and terminal credentials are refused by the gateway; device credentials are refused by the owner's listener. |
| A paired phone | Make the Runner post somewhere (SSRF) through its push endpoint | The endpoint must be `https`, port 443, no credentials, no IP literal, on Apple's, Google's, Mozilla's or Microsoft's push hosts (exact or subdomain); the Runner's push client follows no redirect, uses no proxy, and its dialer refuses loopback, private, link-local, tailnet (100.64/10), "this network" (0/8) and benchmarking (198.18/15) addresses, and the IPv6 prefixes that carry an IPv4 address (NAT64 `64:ff9b::/96`, 6to4 `2002::/16`). |
| The push services (Apple, Google, …) | Read what an agent is doing | The payload is encrypted to the phone's key (RFC 8291, aes128gcm) and holds only the conversation's title, the kind of event and a link — never transcript, command or workspace text. The service sees size and timing. The VAPID `sub` names the project, not the person or the tailnet. |
| A malicious page | Frame or script the phone app | `Content-Security-Policy` with `script-src 'self'`, `frame-ancestors 'none'`, `form-action 'none'`, `no-referrer`, `nosniff`. |

Out of scope, as for agent credentials ([resource ownership](RESOURCE_OWNERSHIP.md#agent-credentials)):
another process running as the same OS user can read the Runner's state
directory. Tailscale's coordination server distributes WireGuard public keys;
its relays (DERP) see only ciphertext. Trusting Tailscale for tailnet
membership is a deliberate dependency of this increment.

## Decision: transport

| Option | Verdict |
| --- | --- |
| (a) LAN listener with TLS and a scoped token | Rejected. A phone browser cannot trust a self-signed certificate without warnings or a profile install; without a trusted origin there is no WebCrypto (secure context), no service worker and no web push. It also puts a Runner port on the LAN, and works on-LAN only. |
| (b) Relay with end-to-end encryption (V1 `crates/relay` + `crates/e2e-channel`, Noise IK) | Deferred to the native iPhone app. In a web page the relay (or whoever serves the page) also serves the code that does the encryption, so it could serve code that leaks the keys: that is not end to end. A signed native app can pin the Runner's key from the QR code, which is exactly V1's Noise IK model. It also needs an operated relay. |
| (c) Tailscale (`tailscale serve`) | **Chosen.** WireGuard between the phone and this machine, end to end; reachable off-LAN; tailnet-only; a real `*.ts.net` certificate, so the phone gets a secure context (WebCrypto, installable app, a path to web push) — with no infrastructure for us to run and no port on the LAN. The cost: the phone must have Tailscale and be in the owner's tailnet. |

Tailscale is the transport, not the authority: a tailnet peer still needs a
paired device credential, and the gateway still refuses the owner's bearer.

## Design

```
phone (Safari) ──WireGuard/HTTPS──▶ tailscale serve :8443 ──▶ device gateway 127.0.0.1:<port>
                                                                 ├─ /                   built phone app (apps/web/dist)
                                                                 ├─ /remote/v1/pair     one-time code + key proof
                                                                 ├─ /remote/v1/challenge, /session  sign-in
                                                                 └─ /lazybox.bench.v1.RunnerService/*  DeviceAuthority only
owner's windows ──▶ Codefly Connect/gRPC listeners on loopback ──▶ LocalAuthority (owner, agent, terminal)
```

- **Gateway** (`internal/remote/gateway.go`): a second loopback listener in
  the Runner process, on the port it used before (`<state>/remote.json`) so the
  Tailscale route survives restarts. Its RPC endpoint is a separate gRPC server
  with `transport.DeviceAuthority` alone, transcoded to Connect like the
  owner's listener. The Tailscale choice is remembered and restored at start;
  the route is never touched if another service owns the port
  (`internal/remote/tailscale.go`). The HTTPS port is `remote.httpsPort` in the
  Runner configuration (default 8443).
- **Pairing** (`internal/devices`): `BeginDevicePairing` (owner only) creates
  an offer: 128-bit id, 256-bit secret kept only as SHA-256, five minutes. The
  QR link is `https://<host>:8443/?view=remote#pair=<id>.<secret>&grants=…` —
  the secret is in the fragment, which browsers never send, so it reaches no
  server or proxy log, and the phone removes it from history at once. The
  phone generates a non-extractable ECDSA P-256 key and sends its SPKI public
  key, a name, and a signature over `lazybox-device-pair-v1\0<id>\0<key>`. The
  offer is consumed atomically; a used, expired, cancelled or burned offer
  pairs nothing.
- **Sign-in**: the phone asks for a challenge (32 random bytes, 60 s, single
  use, bound to the device, at most 8 pending per device) and signs `lazybox-device-session-v1\0<device>\0<challenge>`.
  The answer consumes the challenge whatever the outcome. The credential is
  `lbx-device.<device>.<expiry>.<HMAC>` under `<state>/device-credential.key`,
  checked on every call against the device record: revoking deletes the
  record, so the next call fails. A stream (`WatchEvents`, `WatchOutput`)
  is bound to the credential that opened it: it ends `Unauthenticated` at
  the credential's expiry (the phone, which renewed 90 s earlier, reconnects
  from its cursor) and at once when the device is revoked. The phone renews 90 s before expiry, and on
  any refused call through the existing `RunnerLink` renew path.
- **Records**: `<state>/devices.json` (0600): id, name, public key, grants,
  paired and last-seen times (written at most once a minute). No credential is
  ever written to disk.

### Push (R7p)

```
Runner ─ every 3 s: conversations ─▶ Notifier ─ new request / failure / turn end ─▶ per phone: kinds, quiet hours, 8/min
                                       └─ encrypt (RFC 8291) + VAPID JWT ─▶ phone's push service ─▶ service worker ─▶ notification
```

- **Keys**: `<state>/vapid.key` (0600), a P-256 key made once and never
  replaced (`internal/webpush`). Its public half is the phone's
  `applicationServerKey`, returned only to a device's own `GetPushSettings`.
- **Subscriptions**: on the device record in `devices.json` (`push`), with the
  phone's kinds, quiet hours and time zone, last sent and last error. Revoking
  the device removes it; a push service's 404/410 clears it (only that
  subscription, so a newer one survives); the phone hands its current
  subscription to the Runner once each time the app opens (not on each
  credential renewal), so a rotated one heals itself. It sends the
  subscription alone: the preferences set in Settings are never restated.
- **What notifies** (`internal/remote/notify.go`) is the desktop's attention
  rule (`attention.ts`) ported: a request id the conversation did not have
  before, a turn into failure with new activity, a root turn ending. The first
  look after a start only records, so a Runner restart never re-notifies.
  More than three at once become one "N conversations need you". Needs input
  is sent with `Urgency: high` and a 1 h TTL; each conversation has its own
  `Topic`, so an offline phone receives only its newest.
- **Payload**: `{"title","kind","url","tag"}` — kinds `approval`, `question`,
  `failed`, `review`, `summary`, `test`; `url` is `/?view=remote&session=<id>`;
  `tag` is the conversation, so a banner replaces its predecessor.
- **Service worker** (`apps/web/public/sw.js`, served `no-cache`): shows every
  push (iOS and Chrome require it), opens or focuses the app on a tap and
  takes it to the conversation without a reload, and keeps the app shell
  (network first) so the phone opens Lazybox and says the computer is
  unreachable rather than showing the browser's error page. It never caches
  the API. The page closes banners whose conversation no longer waits.
- **Pins** are the computer's: the Runner keeps them on each conversation
  (`Session.pinned_at`, `PinConversation`), and the phone reads them through
  `runnerPinStore` (`remote/pins.ts`, behind the `PinStore` interface). A
  phone with `respond` pins and unpins them; a read-only phone shows them.
  Pins a phone kept on itself before move to the Runner once (a read-only
  phone keeps showing them beside the computer's). Conversations archived on
  the computer are not listed, unless one waits for the person. Read marks
  are still the phone's own.

### What a device may call

Pinned by `TestDeviceAllowlistIsPinned`; every other RPC is denied to a device
holding every grant (`TestDeviceCredentialIsDeniedEveryRPCOffTheAllowlist`).

| Grant | RPCs |
| --- | --- |
| `read` (always) | `Info` (redacted), `ListHistory`, `GetSession`, `ListCollections`, `GetCollection`, `ListWorkspaceChanges`, `ReadWorkspaceDiff`, `ListCommands`, `ReadCommand`, `ReadAttachment` (images a sent message carried); `ReadSessionTail` (a conversation's latest turns and earlier pages: what `WatchEvents` replays, in fewer reads); `GetPushSettings`, `SetPushSettings` (its own only, at most 30 changes a minute; the owner is refused) and `SendTestPush` (itself only, within its 8-a-minute notification budget; never a hook), so a read-only phone is notified too; `ListLinks` (the tracked pull requests, never as an agent); streams `WatchEvents`, `WatchOutput` |
| `respond` (default) | `Apply` with message, steer, interrupt, answer or approval — never permission; `PinConversation` (the person's own arrangement, shared with the computer; never archive or delete); `SendPullRequestFailures` and `ImportPullRequestReview` (a tracked pull request's failures or review, to the conversation that delivered it — never merge, link or unlink); `AnswerConversationStart` (an agent's request to start a conversation: start it as asked, or decline — never another agent, model or workspace) |
| `start` (opt-in; not offered at pairing until the phone has its screen) | `OpenSession` only in an existing workspace, never a fork, with no directory or resume reference, and no option but the model and its effort: permission, workflow (a phone cannot leave plan-by-default), gateway, instructions, behavior profile, rotation accounts and no-rotation stay the Runner's defaults. The phone still names the account (`profile_id`, which `OpenSession` requires); `ResumeSession`, never with `processes_stopped` (the owner's statement after an unclean shutdown) |

## Verification

- Go: `internal/devices` (offer expiry, single use, burn after wrong secrets,
  proof of key, challenge replay/expiry/cross-device, forged/rebound/extended
  credentials, revoke, restart persistence, file modes, a real WebCrypto
  signature vector); `internal/transport` (pinned allowlist, exhaustive
  denial over the generated service description, per-grant enforcement,
  approval from a phone while agents still cannot, credentials do not cross
  listeners, revoke, Info redaction, Connect); `internal/remote` (HTTP pairing
  journey, JSON/POST/size limits, security headers and Funnel refusal on every
  path, static serving without traversal, loopback bind and port reuse,
  Tailscale states, HTTPS-only/loopback-only serve, never replacing another
  service, restore after restart).
- `scripts/remote-device.test.ts`: the phone's key is not extractable, only
  the public key leaves it, challenges are not reused, a used code and a
  revoked device are reported.
- Playwright against the fake Runner (`test:remote-pairing`,
  `test:remote-inbox`): the Settings flow and QR code, the phone pairing with
  real WebCrypto signatures checked by the fixture, matching fingerprints,
  replay refused, expiry, withdrawal, revoke signing the phone out; the phone
  inbox, approve, answer, reply, interrupt, back gesture, read-only phone, 320
  and 390 px widths, and that the phone sent only device credentials and
  called only allowlisted RPCs.
- Push, Go: `internal/webpush` (the RFC 8291 Appendix A vector byte for byte,
  round trip through a receiving-side decrypt, VAPID JWT signature, `aud`,
  `exp`, `sub`, endpoint allowlist and SSRF refusals, key file mode and never
  replacing a key, push-service status codes); `webpushtest` is a fake push
  service that decrypts and checks VAPID as the real ones do;
  `internal/devices` (subscription and preferences validation, persistence,
  revoke removes it, gone clears only the failed subscription);
  `internal/remote/notify` (first look silent, new request once, failure and
  turn-end transitions, per-kind toggles, quiet hours across midnight and time
  zones, rate limit, burst summary, gone subscription cleared, revoked device
  gets nothing, no transcript or command text in a payload); `internal/transport`
  (a phone sets and reads only its own push, cannot test another, the owner
  is refused Get/Set and may test a named device, listing shows push without
  the key; the pinned allowlist).
- `scripts/remote-phone.test.ts`: push support (iPhone tab → Home Screen,
  iOS < 16.4, iPadOS, Android), quiet hours, what a row offers for each
  request (plain, flagged, default-to-no, diff, settings, chips, multi,
  several questions, typed answers), pins, routes.
- Playwright `test:remote-push` (Chromium at 390 px, the full Chromium in
  headless mode, which has notifications): row Allow with Undo then sent from
  the device, the request gone on the computer; flagged command confirmed
  (Cancel sends nothing); Deny with a reason; chips single and multi; swipe
  right pins with Undo, long left swipe marks read with Undo, left tray Stop
  confirms then interrupts; ⋯ sheet; pull to refresh; offline bar; a running
  command's output following live; the permission flow and subscription with
  a mocked `PushManager`; kinds, quiet hours with the time zone, test push and
  its failure; a needs-input push delivered to the service worker with the
  page closed (`ServiceWorker.deliverPushMessage`) shown as "Approval needed",
  its click taking the open app to the conversation, approved there behind the
  confirmation; the iPhone Home Screen guidance; revoke ending notifications.
  Screenshots `.cache/remote-*.png`, dark and light.

**Not verified:** a real `tailscale serve` (the tests use a fake CLI; the
command form is `tailscale serve --bg --https=<port> http://127.0.0.1:<port>`
and `tailscale serve --https=<port> off`, as documented by Tailscale), a real
phone on a tailnet, iOS Safari's IndexedDB persistence of non-extractable keys
across app restarts, and the `Tailscale-Funnel-Request` header name, which is
defence in depth on top of refusing Funnel ports.

**Not verified (push; needs a real phone):** delivery through Apple's,
Google's and Mozilla's real push services (Apple's acceptance of the `sub`
claim and the `Topic` header in particular); iOS showing notifications from a
Home Screen app and its permission prompt; a notification tap with no Lazybox
window open (`clients.openWindow` needs a real tap; the test covers the open
app); the subscription surviving iOS restarts; Android Chrome's real
`PushManager` (the tests mock it); swipe feel and haptics on a real touch
screen (Android ticks; iOS Safari has no vibration API); the push client's
dialer against real DNS answers; the notifier's 3 s observation cost on a
Runner with many conversations (it does the work of an `Info` call, and only
while some phone is subscribed).

## Deferred

- **Shared read marks** with the computer (pins are shared). A notification sent while the computer shows that conversation
  is not suppressed (the Runner does not know what a window shows).
- A missed notification across a Runner restart: the first look after a
  start only records, so a request that arrived while the Runner was down is
  in the Inbox but not pushed.
- **Off-tailnet access** through V1's relay with Noise IK, in the native iPhone
  app (see the decision above).
- A phone revoking itself on the Runner ("Forget pairing" only clears the
  phone); per-workspace grants; reviewing diffs and files on the phone (the RPCs
  are readable, the screens are not built); starting a conversation from the
  phone (the grant exists and phones paired with it keep it; pairing no
  longer offers it, since the screen does not exist).
- Hub-level device identity and multi-human attribution (D05).
