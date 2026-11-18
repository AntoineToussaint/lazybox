# Voice settings

**Settings → Voice.** Every option, its default, and what it does. The guide is
[Voice: dictate, say commands, hear replies](../guides/voice.md).

Voice settings are kept on this device, for every window (like Appearance and
Shortcuts). The two voice keys are part of your shortcuts.

## Dictation

| Setting | Default | What it does |
| --- | --- | --- |
| Engine | Automatic | Which speech recognition dictates. The desktop app uses **macOS speech recognition**; a browser uses its own. Shown as a choice only where there is more than one. |
| Permissions | — | Microphone and (on the Mac) Speech Recognition: *Allowed*, *Not asked yet*, *Blocked*. **Allow…** asks; a blocked one has a button to the right page of System Settings. Rechecked when you come back to the window. |
| Language | Automatic | What you speak. *Automatic* is your system's language. On the Mac, a language this Mac can only recognize through Apple's server is marked *needs Apple's server*. |
| Use Apple's server when needed | Off | Mac only. Off: a language not available on-device does not dictate, and audio never leaves the Mac. On: such a language is sent to Apple while you dictate. |
| Privacy note | — | Says where audio goes for the engine and language in use. "On-device" only when the engine confirms it. |

## Wispr Flow

Use **Get Wispr Flow** and **Setup guide** to install and sign in to the
Wispr Flow app. **Try Wispr Flow** is an editable practice field. Its text
is discarded when you leave Voice settings.

## Keys

| Setting | Default | What it does |
| --- | --- | --- |
| Dictate | `Right ⌥` or `⌥Space` | Hold to dictate into the message box you are working in. **Change…** records a new key (press a chord, or press and release `Right ⌥` or `Right ⌘` alone); **Reset** goes back to the default. If `⌥Space` does nothing, another app (ChatGPT, Raycast, Alfred) may use it: pick another key. |
| Say a command | `⌥⇧Space` | Hold to open the command palette and say what to run. |
| How the key works | Hold to talk, tap to keep listening | *Hold to talk*: listening stops when you release. *Press to start, press again to stop*: press again to keep the draft. With `Right ⌥` alone, tap twice to start and once to stop. Press `Esc` to cancel. |

A voice key must be one key held with `⌥`, `⌃` or `⌘` (`Ctrl+Alt+M`), a
function key (`F5`), or `Right ⌥` or `Right ⌘` alone (`AltRight`, `MetaRight`
in `keys.yaml`). A plain letter or a `]]` sequence is refused (you could not
type it), and so is a left modifier alone (it is pressed with other keys all
the time). `Fn` cannot be one: the app never sees it.

`Right ⌥` alone talks only when held about 0.2 s with no other key: `⌥e`, `⌥←`
or an `⌥`-click stay what they are. Keymaps saved before it existed gain it,
first, beside the key they had.

## Text

| Setting | Default | What it does |
| --- | --- | --- |
| Automatic punctuation | On | macOS 13 and later adds commas, periods and question marks as you speak. Not offered by browsers. |
| Spoken punctuation | On | "comma", "period", "full stop", "question mark", "exclamation mark", "colon", "semicolon", "new line", "new paragraph", "open/close parenthesis" — « virgule », « point », « point final », « point d'interrogation », « point d'exclamation », « deux points », « point-virgule », « à la ligne », « nouvelle ligne », « nouveau paragraphe », « ouvrez/fermez la parenthèse ». French words apply when the language is French (*Automatic* follows your system's language); « point » is never punctuation in English. |

## Spoken commands

| Setting | Default | What it does |
| --- | --- | --- |
| Spoken commands | On | The words below act when they **end** what you said (after a short pause, or when you release the key). Off: they are text. A negation just before ("don't send", "ne pas envoyer") keeps them as text. |
| Send when I say "send" | **Off** | On: the send words send the message (without the word). Off: they stop listening, and a note reminds you to press `↵`. |
| Send | send, send it, envoie, envoyer, envoie-le | Comma-separated; case and accents do not matter. |
| New line | new line, nouvelle ligne, à la ligne | Starts a new line; listening goes on. |
| Cancel | cancel, annule, annuler | Drops the dictation; the message box is as it was. |
| Stop | stop, stop listening, arrête, arrête la dictée | Stops listening; the text stays. |

Emptying a list restores its defaults.

## Read aloud

| Setting | Default | What it does |
| --- | --- | --- |
| Agent replies | Off | *First paragraph* or *Whole reply*, read when a turn ends in a conversation you have open (once, even with it open in two windows). History is never read. |
| Conversations that need you | Off | Reads fresh questions, pending approvals, and blocking failures from the main window. Waits until dictation ends and skips resolved requests. |
| Voice | System default | Any voice your system has. |
| Speed | 1.0× | 0.5× to 2×. **Play a sample** reads a sentence in the chosen voice. |

Reading stops with `Esc`, the ■ in the pill, or *Stop reading aloud* in the
command palette, and never happens while you dictate.

## Shortcuts and palette entries

| Action | Keys | Palette |
| --- | --- | --- |
| Dictate | `Right ⌥` or `⌥Space` (held) | *Dictate* starts or stops listening (toggle) |
| Send message | `↵`, also while listening (the last words settle first); while the agent works it queues | — |
| Send now | `⌘↵`, also while listening: while the agent works it stops and sends now, otherwise it sends | *Send now* |
| Say a command | `⌥⇧Space` (held) | *Say a command* |
| Read latest reply | — | *Read latest reply* in the current conversation |
| Repeat or open an announcement | — | *Repeat last announcement* or *Open that announcement* |
| Stop reading aloud | — | *Stop reading aloud*, while something is read |
