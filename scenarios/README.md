# Scenarios

A scenario is a user story played on the real app with mocked data. One file
gives you two things:

- **a video**: the web client (apps/web) recorded at 2×, with a pointer,
  captions and the keys pressed drawn over it. Nobody screen-records.
- **an end-to-end test**: each step checks what it changed, so a scenario that
  renders wrong fails, at the step that went wrong.

The data comes from the fake Runner (`scripts/fixtures/runner-ui.mjs`), the
one the UI suites use. No Git, no model and no network are involved.

```sh
npm run scenario -- --list                     # every scenario, with its story
npm run scenario -- start-from-issue           # MP4 + WebM + contact sheet + report
npm run scenario -- all --gif                  # every scenario, with a GIF each
npm run scenario -- inbox-on-phone --light     # the other theme (or --dark)
npm run scenario -- pr-checks-loop --phone     # the phone layout
npm run test:scenarios                         # all of them as tests, no video (a release gate)
```

The output goes to `.cache/scenarios/` (ignored by Git):
`<name>.mp4`, `<name>.webm`, `<name>.gif` with `--gif`,
`<name>.contact.png` (one frame per caption) and `<name>.report.json` (every
step, its checks and when it happened in the video). A variant adds
`-phone` or `-light` to the name. A failed run also leaves
`<name>-failed.png` and `<name>-failed.aria.yaml`, the page's accessibility
tree, which is what your locators see.

The other options are `--no-video` (run it as a test), `--no-webm`, `--scale=N`
(device pixels per CSS pixel, default 2) and `--timeout=ms` (per check,
default 10000).

`SCENARIO_DEBUG=1` prints each step as it starts, with the page's errors and
their stacks. `SCENARIO_REAL_CLOCK=1` leaves the page's clock real, so you can
tell an artefact of the fixed clock from a bug.

Video needs `ffmpeg` on `PATH`, or `FFMPEG=/path/to/ffmpeg`. Without it the
run says so and writes Playwright's own WebM instead, which is 1× and softer.

## Writing one

Add `scenarios/<name>.scenario.ts`. The file name is the scenario's name.

```ts
import { scenario, caption, press, paste, say, click, expect, agent, button, group, textbox, dialog } from './lib/dsl.ts';

export default scenario({
  title: 'Start a task from a GitHub issue',
  story: { as: 'a developer with an issue to fix', want: 'to start an agent from the issue’s link', soThat: 'it gets the issue, a workspace and a branch' },
  tags: ['github', 'new-task'],
  viewport: 'desktop',            // 1440×900; 'phone' is 390×844
  theme: 'dark',
  build(w) {
    // The world, before the app opens.
    w.connectGitHub();
    const payments = w.workspace('Payments', { repos: ['acme/api', 'acme/web'] });
    const url = w.issue({ repo: 'acme/api', number: 812, title: 'Fix refund rounding', labels: ['bug'] });
    w.open(payments);
    // The timeline.
    const sheet = dialog('New task');
    return [
      caption('Start the task **from its link**'),
      press('Mod+N'),
      paste(url, { into: textbox('First message').in(sheet) }),
      expect.visible(group('From acme/api#812').in(sheet)),
      say('Start with the ledger.', { into: textbox('First message').in(sheet) }),
      press('Enter'),
      agent.receives('message', 'acme/api#812'),
      agent.thinks(1.2),
      agent.runs('go test ./ledger/...', { output: 'ok  acme/api/ledger 0.2s\n' }),
      agent.writes('Fixed: refunds round half to even. **The tests pass.**'),
      agent.done(),
    ];
  },
});
```

Then run it as a test until it passes (`npm run scenario -- <name> --no-video`),
render it, and **watch it**. Open the contact sheet first: a frame per caption
shows at a glance whether each chapter looks right.

### Things to know

- Find things the way a person does: by role and name (`button('Send')`,
  `role('tab', 'Changes')`), then `.in(parent)`, `.first()`, `.withText('…')`.
  Use `css('.pr-pill')` only for what has no name.
- Agent steps journal events the way the Runner journals a real agent's, so
  the app shows what it really shows. `agent` is the conversation on screen.
  A handle from `workspace.conversation()`, or `conversation('Title')` for one
  started later (an orchestrator's child, say), works the same way for any
  conversation.
- Time is the scenario's own. It starts on 17 November 2026 at 15:42 UTC and
  moves only with `wait`, `thinks` and the other agent steps. The page and the
  fake Runner share that clock, so "2 min ago" reads the same on every run.
  Typing speed and pauses come from a generator seeded by the scenario's name.
- A test (`--no-video`) runs the same steps without the pauses, in a few
  seconds. The scenario clock still moves.
- Captions stay until the next one, and at least 2.2 s. Keep them to one
  line. `**bold**` is drawn in the accent colour.
- A page error (an exception in the app) fails the scenario, and the report
  names the step it happened in.

## Reference

**Scenario**: `scenario({ title, story: { as, want, soThat }, tags, viewport, theme, build(w) })`.
`build` returns the steps.

**World** (`w`, in `build`):

| | |
|---|---|
| `w.connectGitHub(login?)` | GitHub connected |
| `w.repository('acme/api')` | a repository the Runner knows |
| `w.workspace(name, { repos, branch? })` | a workspace on a branch → handle |
| `ws.conversation(title, { agent?, model?, queue?, sentNowReply?, permission?, createdMinutesAgo? })` | an idle conversation → handle |
| `conv.history(h => h.prompt().read().bash().edit().plan().reply().end(), { endedMinutesAgo })` | its past (or `conv.history(longConversation())`, raw Runner events) |
| `conv.granted({ title, brief }, { maxChildren, planRequired, budgetUsd })` | may run agents for a mission |
| `ws.file(path, text)` | a file in the workspace |
| `ws.pullRequest({ number, title, conversation, repo?, checks? })` | a pull request, opened later by `agent.opensPR(pr)` |
| `w.issue({ repo, number, title, body, labels, comments })` | a GitHub issue; returns its URL |
| `w.voice()` | dictation without a microphone (see `hears`) |
| `w.open(conv \| ws \| 'start' \| 'phone')` | where the app opens; `'phone'` pairs a phone and opens its Inbox |

**The person**:

| | |
|---|---|
| `say(text, { into?, pace? })` | types, a key at a time (`\n` is ⇧↵) |
| `paste(text, { into? })` | a clipboard paste |
| `press('Mod+K')` | keys, shown as keycaps (`Mod` is ⌘ on a Mac) |
| `click(target)`, `rightClick(target)`, `hover(target)` | the pointer glides there first |
| `scroll(target, dy)` | the wheel, as a flick |
| `hears(words, { final? })` | the microphone hears it (needs `w.voice()`) |
| `visit(path)`, `reload(until, { within })` | navigation; `reload` checks how fast `until` is back |
| `wait(seconds)` | time passes |

**The agent** (`agent`, or a conversation handle):

| | |
|---|---|
| `thinks(s)`, `reads(...paths)`, `edits(path, { from, to })` | |
| `runs(command, { output, exit, seconds, description, console? })` | with `console: { agentView, filter, outputBytes }`, through `lazybox run`: the output streams to the console panel, and the agent gets the compact view |
| `startsRunning(command)` | a command that keeps going (the turn stays busy) |
| `writes(markdown, { stream? })` | streamed; checks the text is on screen |
| `asks({ question, options })`, `requests(command)` | a question, or an approval: the turn waits |
| `receives('message' \| 'answer' \| 'approval', matching?)` | waits until what the person did reached the Runner, and checks it |
| `done()` | the turn ends |
| `opensPR(pr)`, `pushes(pr?)`, `publishes({ title, kind, content })` | |
| `proposesPlan({ summary, tasks })`, `spawns({ title, brief, role })`, `reports(summary)` | orchestration |
| `set(fields)` | any session field |

**The world, changing**: `pr.checksRunning()`, `pr.checksFail(['lint'])`,
`pr.checksPass({ approved })`, `pr.approved()`, `ws.changesFile(path, text)`.

**Narration**: `caption(text, { hold?, position? })`, `uncaption()`,
`highlight(target, { label?, hold? })`.

**Checks**: `expect.visible`, `hidden`, `text`, `exactText`, `count`,
`value`, `focused`, `attribute`, `expect.call(method, body => …)` (the Runner
got that RPC), `expect.runner(label, fixture => …)`.

**Targets**: `role`, `button`, `link`, `tab`, `option`, `menuitem`,
`textbox`, `dialog`, `region`, `group`, `checkbox`, `heading`, `text`,
`label`, `css`, `composer()`, `transcript()`; narrow one with `.in()`,
`.first()`, `.last()`, `.nth()`, `.withText()` or `.or()`.

**Escape hatch**: `custom('label', async ({ page, fixture, world, clock }) => …)`.

## How it works

- `lib/dsl.ts`: the DSL. `lib/world.ts`: the builders and agent steps over the
  fake Runner. `lib/step.ts`: steps and targets.
- `lib/runner.ts`: one Vite server and one browser for the run. Each scenario
  gets a fresh fake Runner and a fresh browser context.
- `lib/overlay.ts`: the pointer, ripples, captions, keycaps and highlights. They
  live in a shadow root that is hidden from accessibility and lets the pointer
  through, so locators never see them. They use the app's theme tokens.
- `lib/recorder.ts`: captures frames at device scale over CDP. The frames come
  as fast as the page renders, about 20 per second at 2880×1800, each with its
  time. ffmpeg turns them into 30 fps H.264 and VP9, the GIF and the contact
  sheet.
- Frames between steps depend on timing. The key frames are taken where the
  scenario rests, at the end of each caption, and their hashes are in the
  report so that you can diff two runs.
