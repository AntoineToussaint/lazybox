import { scenario, caption, click, press, hears, expect, highlight, composer, css, transcript } from './lib/dsl.ts';

export default scenario({
  title: 'Dictate a message instead of typing it',
  story: { as: 'someone who thinks faster than they type', want: 'to talk to the agent', soThat: 'a long instruction takes seconds, and I still see every word before it goes' },
  tags: ['voice', 'composer'],
  viewport: 'desktop',
  theme: 'dark',
  build(w) {
    w.voice();
    const search = w.workspace('Search', { repos: ['acme/web', 'acme/api'], branch: 'perf/search' });
    const conv = search.conversation('Speed up search', { createdMinutesAgo: 50 });
    conv.history(h => h
      .prompt('Search feels slow. Add timing to the search endpoint so we can see where the time goes.')
      .read('api/src/search.ts', 'api/src/db.ts')
      .edit('api/src/search.ts', 'return rows', 'timer.end(); return rows')
      .reply('Added timing: every search logs how long the query, the ranking and the rendering took.')
      .end(), { endedMinutesAgo: 8 });
    w.open(conv);
    const pill = css('.voice-pill', 'the dictation pill');
    return [
      caption('Too long to type? **Say it**: tap ⌥ Space'),
      click(composer()),
      press('Alt+Space'),
      expect.text(pill, 'Listening'),
      highlight(pill, { label: 'Listening', hold: 1.2 }),
      caption('Words appear as you speak, marked until they settle'),
      hears('the search page is slow on phones'),
      hears('find the slowest query and fix it'),
      hears('then measure it again', { final: false }),
      expect.text(pill, '⌘↵ send'),
      caption('Every word is in the box before it goes: **⌘↵** sends'),
      press('Mod+Enter'),
      conv.receives('message', 'The search page is slow on phones find the slowest query and fix it then measure it again'),
      expect.hidden(pill),
      expect.value(composer(), ''),
      caption('The agent got exactly what you said'),
      conv.thinks(1.2),
      conv.runs('npm run bench -- search --device=mobile', { output: 'query 412 ms · rank 38 ms · render 51 ms\n', seconds: 1.4, description: 'Measure search on a phone profile' }),
      conv.writes('The query is the slow part: **412 ms**, a full scan on `products.title`. I’ll add a trigram index and measure again.'),
      conv.edits('api/migrations/0042_products_title_trgm.sql'),
      conv.runs('npm run bench -- search --device=mobile', { output: 'query 21 ms · rank 37 ms · render 50 ms\n', seconds: 1.4, description: 'Measure again' }),
      conv.writes('With the index the query takes **21 ms** instead of 412: search answers in about 110 ms on a phone.'),
      conv.done(),
      expect.text(transcript(), 'about 110 ms on a phone'),
      caption('Seconds to say, nothing lost on the way', { hold: 3 }),
    ];
  },
});
