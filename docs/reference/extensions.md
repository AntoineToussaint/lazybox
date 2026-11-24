# Extension reference

An extension is a folder `extensions/<id>/` in your config folder
(`lazybox extension path`). Guide: [Extensions](../guides/extensions.md).
Design and threat model: [EXTENSIONS.md](../../EXTENSIONS.md).

## `extension.yaml`

Schema: `extensions/extension.schema.json`, written beside your extensions;
the header `lazybox extension new` writes names it, for your editor's
completion.

| Field | Meaning |
| --- | --- |
| `lazybox` | The bridge protocol: `1`. |
| `name` | What Settings and the panel show (48 characters). Required. |
| `version`, `description`, `author` | Shown in Settings and the approval sheet. |
| `homepage` | An `https://` address. |
| `panels` | 1–8 panels: `id` (lowercase, digits, dashes), `title`, `entry` (an `.html` page of the folder, default `index.html`), `icon`, `description`. Optional with an `accountPolicy`. |
| `grants` | What it asks for (below), up to 40. `switch:claude` and `switch:codex` are an account policy's: the providers it may switch. |
| `accountPolicy` | What the Runner does at a subscription limit: `switchOnLimit` (`true` moves the conversation to another signed-in account of its provider) and `accounts` (ids to try, in order; empty is every other one). Needs a `switch:<provider>` grant. See [account policies](../guides/extensions.md#account-policies). |
| `network` | Origins it may fetch from, up to 16: `https://host[:port]`, `https://*.example.com`, or `http://localhost[:port]` / `http://127.0.0.1[:port]`. No paths. |

Icons: `puzzle` (default), `chart`, `columns`, `activity`, `target`, `sliders`,
`globe`, `bell`, `inbox`, `checklist`, `report`, `notes`, `card`, `tag`,
`sparkle`, `shield`, `agent`, `eye`, `history`, `file`, `folder`, `chat`,
`terminal`, `branch`, `cloud`, `review`, `layout`.

Any error leaves the whole extension off (*Does not load*, with the line);
unknown keys are warnings. The folder name is the id; a symbolic link in it,
more than 400 files or 8 MiB (4 MiB a file) make it not load.

## The bridge: `lazybox`

`_lazybox/lazybox.js` defines `window.lazybox`; types are in `lazybox.d.ts`.
Every call returns a promise; a refusal rejects with a `LazyboxError` whose
`code` is `not-granted` (with `grant`), `bad-request`, `unknown-method`,
`unsupported-protocol`, `rate-limited`, `too-large`, `refused`, `failed`,
`unavailable` or `timeout`. Requests are limited to 64 KiB and to about five a
second (twenty at once).

| Call | Grant | Returns |
| --- | --- | --- |
| `lazybox.ready` | — | `{protocol, extension: {id, name, version}, panel: {id, title}, grants, hosts, theme}` |
| `lazybox.workspaces.list()` | `read:workspaces` | `{workspaces: [{id, name, repositories}]}` |
| `lazybox.sessions.list({workspaceId?, limit?})` | `read:sessions` | `{conversations: [{sessionId, conversationId, title, workspaceId, workspaceName, provider, model, status, busy, closed, createdAt, updatedAt}]}` |
| `lazybox.reminders.create(text, due)` | `manage:reminders` | ID of the saved reminder. Supply up to 240 characters and a future Unix timestamp in milliseconds. |
| `lazybox.reminders.list()` | `manage:reminders` | This extension’s reminders for this Runner on this device, ordered by due time. Each has an ID, text and due time. |
| `lazybox.reminders.remove(id)` | `manage:reminders` | Cancels or dismisses the extension’s reminder. |
| `lazybox.attention.list()` | `read:attention` | `{items: [{sessionId, conversationId, title, workspaceId, workspaceName, state, reason, line, at, unread}], counts}` — `state` is `needs-input`, `failed`, `review`, `working` or `idle` |
| `lazybox.commands.list(sessionId)` | `read:commands` | `{commands: [{id, title, state, exitCode?, startedAt}]}` — `running`, `succeeded`, `failed`, `stopped` |
| `lazybox.missions.list()` | `read:missions` | `{missions: [{id, title, canceled, awaitingReview, tasks: [{id, title, status, reason}]}]}` |
| `lazybox.usage.read({workspaceId?, series?})` | `read:usage` | `{totalUsd, unpriced, groups: [{workspaceId, workspaceName, conversationId, model, provider, inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens, usd, records}], series?}` — `inputTokens` is uncached input, the cache's reads and writes apart; `usd` is `null` when unpriced. With `series: true`, also `series: [{at, …the same fields, unpriced}]`: usage hour by hour (`at`, the hour's start in UTC), oldest first, counted as the groups are, so a period's spend is the sum of its hours. |
| `lazybox.conversations.show(sessionId)` | `show:conversations` | `{shown: true}` once the window shows it |
| `lazybox.messages.send(sessionId, text)` | `send:messages` | `{sent: true}` (text up to 32 000 characters) |
| `lazybox.run(action, argument?)` | `do:<action>` | `{done: true, detail}` once the window ran it |
| `lazybox.every(ms, fn)` | — | Runs `fn` now and every `ms` (at least 1 s; slower while hidden); returns `stop()`. |
| `lazybox.on('theme', fn)` | — | Called with the theme when the person changes it (already applied). |
| `lazybox.call(method, params)` | the method's | The raw call. |

Times are Unix milliseconds. `do:` actions are those `lazybox do --list` marks
safe (show, focus or arrange): `inbox.open`, `attention.next`,
`layout.maximize`, `changes.show`, …

## The theme

`lazybox.js` sets every theme token as a CSS custom property on `<html>`
(`--bg`, `--surface`, `--surface-raised`, `--line`, `--text`, `--text-muted`,
`--accent`, `--accent-solid`, `--ok`, `--warn`, `--danger`, `--chart-1` …
`--chart-7`, and the rest of the [token catalogue](../../APPEARANCE.md)), plus
`--font-ui`, `--font-mono` and `--text-scale`, and `data-scheme="dark|light"`.
`_lazybox/lazybox.css` styles the page, buttons (`button.primary`) and
`.card` with them.

## What the page may load

| | Allowed |
| --- | --- |
| Scripts, styles, fonts, media | The extension's own files and `_lazybox/` |
| Images | Its own files, `data:` and `blob:` |
| `fetch`, XHR, WebSocket | The `network:` hosts you approved |
| Frames, workers, plugins, forms, popups, top navigation | Nothing |
| Inline scripts and styles, `eval` | Nothing |
| Navigating its own frame | Its own pages only |

## `lazybox extension`

| Command | What it does |
| --- | --- |
| `lazybox extension new <id> [--name N] [--example agent-board\|cost-dashboard]` | A starting extension (or a copy of an example) in the extensions folder; never replaces one. |
| `lazybox extension validate [<id>\|PATH…]` | Each extension as the Runner reads it: problems as `file:line`, exit 2 when one does not load. |
| `lazybox extension list` | Each extension and whether it is on, needs approval, asks for more or does not load. |
| `lazybox extension path` | The extensions folder. |

Approving happens in the app (Settings → Extensions, or the Extensions panel).
