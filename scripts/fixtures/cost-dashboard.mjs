const HOUR = 3600000, DAY = 24 * HOUR;
// The browser runs in UTC, so the dashboard's local days are UTC days here.
const now = Date.now(), today = now - (now % DAY), thisHour = now - (now % HOUR);

// Dollars per million tokens: input, cache read, cache write, output.
const rates = { 'claude-opus-4-6': [5, 0.5, 6.25, 25], 'claude-sonnet-4-6': [3, 0.3, 3.75, 15], 'gpt-5.1-codex': [1.25, 0.125, 0, 10], 'claude-haiku-4-5': [1, 0.1, 1.25, 5] };
const conversations = [
  { id: 'alpha', workspace: 'alpha', title: 'Fix the parser', models: ['claude-opus-4-6', 'claude-haiku-4-5'], days: [0, 1, 2, 3, 5, 8, 13, 20], weight: 1.6 },
  { id: 'c-alpha-2', workspace: 'alpha', title: 'Refactor the cache layer', models: ['claude-sonnet-4-6'], days: [0, 2, 4, 6, 9, 15, 33], weight: 1.1 },
  { id: 'c-alpha-3', workspace: 'alpha', title: 'Write the migration guide', models: ['gpt-5.1-codex'], days: [1, 3, 10, 11, 12, 26, 40], weight: 0.6 },
  { id: 'beta', workspace: 'beta', title: 'Ship the release', models: ['claude-sonnet-4-6', 'claude-haiku-4-5'], days: [0, 4, 5, 6, 7, 18, 19, 27], weight: 1.2 },
  { id: 'c-beta-2', workspace: 'beta', title: 'Investigate the flaky CI job on main', models: ['gpt-5.1-codex', 'local-llama'], days: [2, 3, 16, 17, 30, 41], weight: 0.8 },
  { id: 'c-beta-3', workspace: 'beta', title: 'Upgrade dependencies', models: ['claude-opus-4-6'], days: [8, 9, 22, 23, 24, 36, 44], weight: 1.4 },
];
let seed = 7;
const random = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
const series = [];
for (const c of conversations) for (const day of c.days) for (const [m, model] of c.models.entries()) {
  for (let k = 0; k < 2; k++) {
    // Today's turns are in hours that have begun; other days' at 09:00 and 15:00.
    const at = day === 0 ? Math.max(today, thisHour - (k + m) * HOUR) : today - day * DAY + (9 + k * 6 + m) * HOUR;
    const input = Math.round((20 + random() * 60) * 1000 * c.weight), cacheRead = Math.round(input * (3 + random() * 6)), cacheWrite = Math.round(input * 0.3), output = Math.round(input * (0.15 + random() * 0.2));
    const rate = rates[model];
    const usd = rate ? (input * rate[0] + cacheRead * rate[1] + cacheWrite * rate[2] + output * rate[3]) / 1e6 : null;
    series.push({ at, workspace: c.workspace, conversation: c.id, model, provider: model.startsWith('gpt') ? 'codex' : model === 'local-llama' ? 'ollama' : 'claude', input, cacheRead, cacheWrite, output, usd, records: 1 + Math.floor(random() * 4) });
  }
}
const nanos = usd => String(Math.round(usd * 1e9));
const tokensOf = b => ({ input: String(b.input), output: String(b.output), cacheRead: String(b.cacheRead), cacheWrite5m: String(b.cacheWrite) });
function report(workspace) {
  const mine = series.filter(b => b.workspace === workspace);
  const groups = new Map();
  for (const b of mine) {
    const key = `${b.conversation}/${b.model}`;
    const g = groups.get(key) || { scope: { workspace, conversation: b.conversation, provider: b.provider }, model: b.model, input: 0, output: 0, cacheRead: 0, cacheWrite: 0, usd: null, records: 0, unpriced: 0 };
    g.input += b.input; g.output += b.output; g.cacheRead += b.cacheRead; g.cacheWrite += b.cacheWrite; g.records += b.records;
    if (b.usd === null) g.unpriced += b.records; else g.usd = (g.usd || 0) + b.usd;
    groups.set(key, g);
  }
  return {
    groups: [...groups.values()].map(g => ({ scope: g.scope, model: g.model, source: 'gateway', tokens: tokensOf(g), ...(g.usd === null ? {} : { nanos: nanos(g.usd) }), records: String(g.records), unpriced: String(g.unpriced) })),
    contexts: [], recent: [],
    series: mine.sort((a, b) => a.at - b.at).map(b => ({ hourUnixMillis: String(b.at), conversation: b.conversation, provider: b.provider, model: b.model, tokens: tokensOf(b), ...(b.usd === null ? {} : { nanos: nanos(b.usd) }), records: String(b.records), unpriced: String(b.usd === null ? b.records : 0) })),
  };
}

export { DAY, today, series, conversations, report };
