# Voice: dictate, say commands, hear replies

Talk to your agents instead of typing: hold a key, speak, let go. The words
appear in the message box as you say them, you fix what you want, and you send.
You can also say a command ("next agent that needs me", "open changes") and have
replies read to you.

The automatic engine on macOS recognizes speech on your Mac. Lazybox does not save dictation recordings.

## Dictate a message

For direct Wispr Flow dictation, configure `LAZYBOX_WISPR_API_KEY` in the
Runner environment with an approved [Wispr Voice Interface API key](https://api-docs.wisprflow.ai/quickstart).
Select **Wispr Flow** under **Settings → Voice → Engine**, then click the
composer microphone. Click **Stop dictation** to close the microphone and
insert the transcript into your draft. Press Esc to discard a recording.
The finished recording goes through this Runner to Wispr Flow.

1. Click into a conversation's message box (or just have one on screen).
2. **Hold `Right ⌥`** — the Option key on the right of the space bar, on its
   own — and speak. A small pill shows a red recording light, the microphone
   level and the time; your words appear in the message box, underlined while
   they are still being recognized.
3. **Release it.** The words settle into ordinary text. Edit them, then press
   `↵` (or `⌘↵`) to send. While the agent works, `↵` queues the message
   and `⌘↵` sends it now.

Tap `⌥Space` to start listening and tap it again to keep the draft. Hold and release
`⌥Space` to keep the text for editing. Pausing to think is fine: what
you said before the pause stays, and the words after it follow.
When a browser yields an empty speech result during a pause, Lazybox keeps
the words it already heard.

`Right ⌥` with any other key is still an ordinary `⌥`: `⌥e` for an accent or
`⌥←` to move by word starts nothing, and a quick tap does nothing either.

Dictated words go where the cursor was, with spaces and capitals as if you had
typed them: "Look at" + *the logs* gives "Look at the logs"; after a full stop
the next word gets a capital.

| To… | Do this |
| --- | --- |
| Keep listening without holding the key | **Tap `Right ⌥` twice** (or tap `⌥Space` once); tap `Right ⌥` or `⌥Space` to stop, or click ■ in the pill to keep the draft |
| Send what you said | `⌘↵` (or `↵`) while listening or after: the last words settle, then the message goes (while the agent works, `⌘↵` sends it now and `↵` queues it) |
| Use the mouse | Click the **microphone button** next to Snippets to start. Click the same button again to keep the draft, or click ■ in the pill to keep the text for editing. |
| Throw away what you just said | `Esc`, the **×** in the pill, or say "cancel" / "annule" |
| Take over with the keyboard | Just type: the words stay and listening stops |
| Move the pill | Drag it; it stays where you put it |

Send explicitly with the keyboard, the Send button, or an enabled spoken send command. With Vim editing on, it works in
any mode: in normal mode the words go after the cursor, as with `a`, and `Esc`
cancels the dictation without changing your mode.

### When the key does nothing

Another app's global shortcut takes a key before Lazybox sees it: `⌥Space` is
the launcher of the ChatGPT app, and a common choice for Raycast and Alfred
(`⌘Space` is Spotlight, `⌃Space` switches the input source). Lazybox cannot
tell, so until the key has worked once the pill says so when you start from
the microphone button. Use `Right ⌥`, or pick another key in **Settings →
Voice → Keys**.

### Spoken commands

Say one of these at the **end** of what you dictate (after it, pause for a
moment or release the key):

| Say | English | Français | What happens |
| --- | --- | --- | --- |
| Send | "send", "send it" | « envoie », « envoyer », « envoie-le » | Sends the message — only if **Send when I say "send"** is on (off by default). Otherwise listening stops and you press `↵`. |
| New line | "new line" | « nouvelle ligne », « à la ligne » | Starts a new line and keeps listening |
| Cancel | "cancel" | « annule », « annuler » | Drops what you dictated and stops |
| Stop | "stop", "stop listening" | « arrête », « arrête la dictée » | Stops and keeps the text |

A command only counts at the end: "send the logs to the agent" stays text, and
so does "don't send". Change the words, or turn commands off, in
**Settings → Voice**.

### Punctuation

On macOS 13 and later the Mac punctuates as you speak. You can also **say**
punctuation: "comma", "period", "question mark", "colon", "new paragraph" —
or « virgule », « point », « point d'interrogation », « deux points »,
« nouveau paragraphe ». (French gets its space before `?` `!` `:` `;`.)

## Say a command

**Hold `⌥⇧Space`**, say what you want, release. The command palette opens and
fills in as you speak:

- "next agent that needs me" → *Next conversation that needs you*
- "open changes" → the Changes panel
- "switch to Beta" → the *Beta release* workspace
- « ouvre les réglages » → Settings

A clear match runs at once, and a small note says what you said and what ran.
When several things match, they stay in the palette: pick one with `↑` `↓` and
`↵`, or say it again. Your own actions (from `actions.yaml`) and anything that
cannot be undone **always ask first**: say "yes" / « oui » (hold the key again)
or press `↵`.

The microphone button in the palette's search field does the same with a click.
With the palette already open, `⌥Space` talks to the palette too.

## Read aloud

**Settings → Voice → Read aloud** can read to you:

- **Agent replies** — the first paragraph or the whole reply, when a turn ends in
  a conversation you have open. Code blocks are named ("code block"), not read;
  links are read as their text.
- **Conversations that need you** — fresh questions, pending approvals, and failures that need attention. Short
  choices can be read with the question. If you are dictating, Lazybox waits
  until you finish and skips requests that have already been resolved.

Use **Read latest reply**, **Repeat last announcement**, or **Open that
announcement** in the command palette when you want to follow up. Say “open
that” with the voice command shortcut to jump to the conversation that was
announced. On a paired phone, tap the speaker in a conversation header to
read its latest reply.

Pick a voice and a speed, and **Play a sample**. `Esc` (or ■ in the pill, or
*Stop reading aloud* in the command palette) stops reading. Nothing is read
while you dictate.

## Where it works

| | Dictation | On-device | Read aloud |
| --- | --- | --- | --- |
| **Desktop app (macOS)** | macOS speech recognition | **Yes** for every language your Mac has for dictation | macOS voices |
| **Browser** (Chrome, Edge, Safari) | The browser's speech recognition | Only where the browser says so — Chrome usually sends audio to Google, Safari to Apple | The browser's voices |
| **Phone** (paired web app) | The phone browser's speech recognition: tap the microphone to start, then tap it again to keep the draft | As in the browser | Read the latest reply on demand |
| **Firefox**, or iOS Safari without Siri | No speech recognition: the microphone button is hidden | — | Firefox reads aloud |

On a phone without the button, use the **microphone on the keyboard**: iOS and
Android dictate into the message box like any text field.

## Privacy

- The microphone is on **only while the recording light shows** in the pill —
  from the moment you start until you release, stop or cancel.
- **No audio is ever saved**, by the app or the Runner. Only the text you keep
  reaches the conversation, and only when you send it.
- On the Mac, recognition is **on-device by default**. A language your Mac
  cannot recognize itself does not dictate unless you turn on **Use Apple's
  server when needed**; Settings → Voice then says the audio goes to Apple.
- **Settings → Voice** says plainly where audio goes for the engine and
  language in use. It says "on-device" only when the engine confirms it.
- Web pages you open in a **Web panel** never get the microphone.

## First time on the Mac

The first dictation asks macOS for two permissions: **Speech Recognition** and
the **Microphone**. Say yes to both. If you said no, **Settings → Voice** shows
which one is blocked with a button that opens the right page of System
Settings → Privacy & Security; switch Lazybox on there and come back — Settings
checks again by itself.

To dictate in a language on-device, it must be installed for dictation:
**System Settings → Keyboard → Dictation → Languages**.

## Settings → Voice

Every option, with its default, is in the [Voice settings
reference](../reference/voice-settings.md). The keys can be changed there (or in
**Settings → Shortcuts**): one key held with `⌥`, `⌃` or `⌘`, a function key,
or `Right ⌥` or `Right ⌘` alone (`AltRight`, `MetaRight` in `keys.yaml`). A
left modifier alone is refused: it is pressed with other keys all the time.

## When something is wrong

| You see | Do this |
| --- | --- |
| "Allow speech recognition for Lazybox…" or "…the microphone…" | Settings → Voice → the button next to the blocked permission |
| "This Mac cannot recognize that language on-device" | Add it in System Settings → Keyboard → Dictation, pick another language, or allow Apple's server |
| "Nothing was heard" | Check the input device in System Settings → Sound, then hold the key and speak |
| The dictation key does nothing | Another app has it (see [When the key does nothing](#when-the-key-does-nothing)): use `Right ⌥` or pick another key |
| "This browser has no speech recognition" | Use Chrome, Edge or Safari, the desktop app, or the keyboard's microphone |
| Words go to the wrong conversation | Click into the message box you want first: dictation goes to the one with the focus, else the one you used last |
