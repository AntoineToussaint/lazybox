import { scenario, composer, caption, say, press, click, expect, reload, scroll, wait, button, region, css, text, transcript, type History } from './lib/dsl.ts';

const TURNS = 140;
const FILES = ['TranscriptView.tsx', 'transcript-model.ts', 'MessageList.tsx', 'useConversation.ts', 'scroll-anchor.ts', 'RichContent.tsx', 'tool-lines.ts'];

/** A long refactor, turn after turn: reads, a test run with real output, an answer with code. */
function longRefactor(h: History) {
  for (let turn = 1; turn <= TURNS; turn++) {
    const file = FILES[turn % FILES.length];
    h.prompt(turn === 1 ? 'The transcript gets slow on long conversations. Refactor how it renders, step by step, and keep the tests green.' : `Next step: ${['split', 'memoize', 'virtualize', 'test', 'simplify', 'document', 'measure'][turn % 7]} ${file}.`);
    h.read(`web/src/transcript/${file}`, `web/src/transcript/${FILES[(turn + 2) % FILES.length]}`);
    h.bash('npm test -- transcript', `${Array.from({ length: 40 }, (_, i) => `✓ transcript ${turn}.${i} renders in order (${2 + (i % 5)} ms)`).join('\n')}\n\nTest Files  12 passed (12)\n     Tests  ${180 + turn} passed (${180 + turn})\n`, { description: 'Run the transcript tests' });
    h.reply(turn === TURNS
      ? `## Step ${turn}: done\n\nThe transcript now renders only what is on screen, keeps its scroll anchor across updates, and reuses parsed Markdown.\n\n| Conversation | Before | After |\n|---|---|---|\n| 40 turns | 180 ms | 38 ms |\n| ${TURNS} turns | 1.9 s | 61 ms |\n\n**All ${180 + turn} transcript tests pass.**`
      : `## Step ${turn}\n\nChanged \`${file}\`: ${['moved the parsing out of render', 'memoized each message by its id', 'kept the scroll anchor on the last read message', 'merged consecutive tool lines', 'dropped a second pass over the events'][turn % 5]}.\n\n\`\`\`ts\nexport const rows = (events: Event[]) => fold(events, ${turn});\n\`\`\`\n\n${180 + turn} tests pass.`);
    h.end();
  }
}

export default scenario({
  title: 'A long conversation opens instantly; Send now when it can’t wait',
  story: { as: 'someone deep in a long session with an agent', want: 'the conversation to reopen at once and my urgent message to get through', soThat: 'I never wait on the app, or on a step I already know is wrong' },
  tags: ['transcript', 'performance', 'queued-messages'],
  viewport: 'desktop',
  theme: 'dark',
  build(w) {
    const web = w.workspace('Transcript speed', { repos: ['acme/web'], branch: 'perf/transcript' });
    const refactor = web.conversation('Make the transcript fast', { createdMinutesAgo: 320, queue: true, sentNowReply: 'Stopped the dev server. Running only the unit tests now.' });
    refactor.history(longRefactor, { endedMinutesAgo: 6 });
    w.open(refactor);
    const strip = region('Queued messages');
    return [
      caption(`${TURNS} turns, ${TURNS * 3} tool calls: it opens at the latest answer`),
      expect.visible(text(`All ${180 + TURNS} transcript tests pass.`)),
      caption('Reload: it is back before you blink'),
      reload(text(`All ${180 + TURNS} transcript tests pass.`), { within: 4 }),
      caption('Every earlier turn is still there'),
      scroll(transcript(), -2600, { seconds: 1.4 }),
      wait(0.8),
      scroll(transcript(), 2600, { seconds: 1 }),
      caption('Ask for one more step'),
      say('Check it in the browser too.'),
      press('Enter'),
      refactor.receives('message', 'Check it in the browser'),
      refactor.thinks(1),
      refactor.startsRunning('npm run dev', { description: 'Start the dev server' }),
      caption('The agent starts a dev server you don’t need: your next message waits…'),
      say('Skip the dev server, the unit tests are enough.'),
      press('Enter'),
      expect.visible(strip),
      expect.text(strip, 'Skip the dev server'),
      caption('…until you press **⌘↵**, Send now: it stops that step and goes next'),
      expect.visible(button('Send now').in(strip)),
      click(composer()),
      press('Mod+Enter'),
      expect.call('Apply', body => body.deliverQueued?.how === 'now'),
      expect.hidden(strip),
      expect.exactText(css('.stopped-line').last(), 'Stopped to send your message'),
      expect.visible(text('Stopped the dev server. Running only the unit tests now.')),
      refactor.runs('npm test -- transcript', { output: `Tests  ${181 + TURNS} passed\n`, seconds: 1.5, description: 'Run the transcript tests' }),
      refactor.writes(`Done without the dev server: **all ${181 + TURNS} transcript tests pass.**`),
      refactor.done(),
      caption('No waiting on the app, or on a step you already know is wrong', { hold: 3 }),
    ];
  },
});
