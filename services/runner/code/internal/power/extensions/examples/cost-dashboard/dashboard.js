// Cost dashboard: an example Lazybox extension (EXTENSIONS.md). It reads token
// use and cost hour by hour (read:usage, `series: true`) and the
// conversations' titles (read:sessions), and shows what was spent in a period:
// the total against the period before, tokens and the cache's share, spend
// over time stacked by model, and where it went by model, workspace and
// conversation. A conversation's row shows it in the window when the panel may
// (show:conversations); a model's or a workspace's row narrows the dashboard.
// Plain DOM and a hand-drawn SVG; every colour is a theme token.
'use strict';

const $ = id => document.getElementById(id);
const SVG = 'http://www.w3.org/2000/svg';
const HOUR = 3600000;
const SLOTS = 5; // models with a colour of their own (--chart-1…5); the rest are "Other" (--chart-7)

const PERIODS = {
  today: { label: 'today', title: 'Spent today', compare: 'yesterday by now' },
  week: { label: 'the last 7 days', title: 'Spent in the last 7 days', compare: 'the 7 days before', days: 7 },
  month: { label: 'the last 30 days', title: 'Spent in the last 30 days', compare: 'the 30 days before', days: 30 },
  all: { label: 'so far', title: 'Spent so far' },
};

const state = {
  period: 'week',
  usage: null,
  conversations: new Map(), // conversationId → {sessionId, title, workspaceName}
  canShow: false,
  filter: { workspace: null, model: null },
  expanded: new Set(),
  slots: new Map(), // model → 1…5, or 0 for Other
  active: -1, // the chart column the pointer or the keyboard is on
  view: null,
};

// ---- Formatting.
const usd2 = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 2, maximumFractionDigits: 2 });
const usd0 = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', maximumFractionDigits: 0 });
const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });
const whole = new Intl.NumberFormat(undefined);
function dollars(value) {
  if (value === null || value === undefined || Number.isNaN(value)) return '–';
  if (value > 0 && value < 0.005) return '<$0.01';
  return (value >= 1000 ? usd0 : usd2).format(value);
}
function axisDollars(value, step) {
  if (value >= 1000) return '$' + compact.format(value);
  return '$' + (step < 1 ? value.toFixed(step < 0.1 ? 2 : 1).replace(/\.?0+$/, '') || '0' : whole.format(value));
}
const tokens = value => value >= 10000 ? compact.format(value) : whole.format(value);
function percent(part, total) {
  if (!total) return '–';
  const p = part / total * 100;
  return p > 0 && p < 1 ? '<1%' : Math.round(p) + '%';
}
const plural = (n, one, many) => `${whole.format(n)} ${n === 1 ? one : many}`;

// ---- Time: local days, so "today" is the person's.
const dayStart = (t, add = 0) => { const d = new Date(t); return new Date(d.getFullYear(), d.getMonth(), d.getDate() + add).getTime(); };
const weekStart = t => { const d = new Date(dayStart(t)); return dayStart(d.getTime(), -((d.getDay() + 6) % 7)); };
const dateLabel = (t, opts) => new Date(t).toLocaleDateString(undefined, opts);
const hourLabel = t => new Date(t).toLocaleTimeString(undefined, { hour: 'numeric' });

/** The period's bounds, the one before it, and its chart columns. */
function windowOf(period, now, first) {
  const today = dayStart(now), tomorrow = dayStart(now, 1);
  const bins = [];
  if (period === 'today') {
    for (let h = 0; h < 24; h++) { const start = new Date(today).setHours(h); bins.push({ start, end: new Date(today).setHours(h + 1), label: hourLabel(start), long: `${hourLabel(start)} – ${hourLabel(new Date(today).setHours(h + 1))}` }); }
    return { start: today, end: tomorrow, prev: [dayStart(now, -1), now - 24 * HOUR], unit: 'hour', bins };
  }
  if (period === 'all') {
    const start = first === undefined ? today : dayStart(Math.min(first, now));
    const days = Math.round((tomorrow - start) / 86400000);
    if (days > 62) {
      for (let s = weekStart(start); s < tomorrow; s = dayStart(s, 7)) bins.push({ start: s, end: dayStart(s, 7), label: dateLabel(s, { month: 'short', day: 'numeric' }), long: 'Week of ' + dateLabel(s, { month: 'short', day: 'numeric', year: 'numeric' }) });
      return { start: bins[0].start, end: tomorrow, prev: null, unit: 'week', bins };
    }
    for (let s = dayStart(Math.min(start, dayStart(now, -6))); s < tomorrow; s = dayStart(s, 1)) bins.push(dayBin(s));
    return { start: bins[0].start, end: tomorrow, prev: null, unit: 'day', bins };
  }
  const days = PERIODS[period].days, start = dayStart(now, 1 - days);
  for (let s = start; s < tomorrow; s = dayStart(s, 1)) bins.push(dayBin(s));
  return { start, end: tomorrow, prev: [dayStart(start, -days), start], unit: 'day', bins };
}
const dayBin = s => ({ start: s, end: dayStart(s, 1), label: dateLabel(s, { month: 'short', day: 'numeric' }), long: dateLabel(s, { weekday: 'short', month: 'short', day: 'numeric' }) });

// ---- Summing what a period holds.
function blank() { return { usd: 0, input: 0, cacheRead: 0, cacheWrite: 0, output: 0, records: 0, unpriced: 0 }; }
function add(into, row) {
  into.usd += row.usd || 0; into.input += row.inputTokens; into.cacheRead += row.cacheReadTokens;
  into.cacheWrite += row.cacheWriteTokens; into.output += row.outputTokens; into.records += row.records;
  into.unpriced += row.unpriced || 0;
  return into;
}
const passes = row => (!state.filter.workspace || row.workspaceId === state.filter.workspace) && (!state.filter.model || row.model === state.filter.model);

function summarize(rows, win) {
  const total = blank(), byModel = new Map(), byWorkspace = new Map(), byConversation = new Map();
  const bins = win ? win.bins.map(() => new Map()) : [];
  const group = (map, key, make) => { let g = map.get(key); if (!g) { g = Object.assign(blank(), make()); map.set(key, g); } return g; };
  for (const row of rows) {
    if (!passes(row)) continue;
    if (win && (row.at < win.start || row.at >= win.end)) continue;
    add(total, row);
    add(group(byModel, row.model, () => ({ key: row.model, provider: row.provider })), row);
    add(group(byWorkspace, row.workspaceId, () => ({ key: row.workspaceId, name: row.workspaceName, conversations: new Set() })), row).conversations.add(row.conversationId);
    if (row.conversationId) add(group(byConversation, row.conversationId, () => ({ key: row.conversationId, workspaceId: row.workspaceId, workspaceName: row.workspaceName, models: new Set() })), row).models.add(row.model);
    if (win) {
      const i = binOf(win.bins, row.at);
      if (i >= 0) { const slot = slotOf(row.model); bins[i].set(slot, (bins[i].get(slot) || 0) + (row.usd || 0)); }
    }
  }
  const ranked = map => [...map.values()].sort((a, b) => b.usd - a.usd || b.output - a.output);
  return { total, models: ranked(byModel), workspaces: ranked(byWorkspace), conversations: ranked(byConversation), bins };
}
function binOf(bins, at) {
  let lo = 0, hi = bins.length - 1;
  while (lo <= hi) { const mid = (lo + hi) >> 1; if (at < bins[mid].start) hi = mid - 1; else if (at >= bins[mid].end) lo = mid + 1; else return mid; }
  return -1;
}
function sumBetween(rows, from, to) {
  let usd = 0;
  for (const row of rows) if (row.at >= from && row.at < to && passes(row)) usd += row.usd || 0;
  return usd;
}

/** Colours follow the model, not its rank in a period: the five that cost most overall keep theirs. */
function assignSlots(rows) {
  const spend = new Map();
  for (const row of rows) spend.set(row.model, (spend.get(row.model) || 0) + (row.usd || 0) + row.outputTokens * 1e-12);
  state.slots = new Map([...spend.entries()].sort((a, b) => b[1] - a[1]).map(([model], i) => [model, i < SLOTS ? i + 1 : 0]));
}
// Usage hour by hour, unless this Lazybox (or its Runner) does not report it:
// then no series at all, or an empty one beside groups that are not empty.
const hasSeries = usage => Array.isArray(usage.series) && (usage.series.length > 0 || !usage.groups.length);
const slotOf = model => state.slots.get(model) || 0;
const modelName = model => model || 'Unknown model';
const workspaceName = id => state.usage?.groups.find(g => g.workspaceId === id)?.workspaceName || 'Workspace';

// ---- DOM helpers (text only: names come from the person's data).
function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}
function svg(tag, attrs, parent) {
  const node = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, String(v));
  if (parent) parent.append(node);
  return node;
}
function say(text) { $('notice').hidden = !text; $('notice').textContent = text || ''; }
function setState(name, title, text) {
  $('page').dataset.state = name;
  if (title) { $('state-title').textContent = title; $('state-text').textContent = text || ''; }
}

// ---- Rendering.
function render() {
  const usage = state.usage;
  if (!usage) return;
  const rows = usage.series || [];
  if (!rows.length && !usage.groups.length) {
    setState('empty', 'Nothing spent yet', 'As your agents work, Lazybox counts their tokens and what they cost. They show here by model, workspace and conversation.');
    return;
  }
  setState('ready');
  const now = Date.now();
  const timed = hasSeries(usage);
  const period = timed ? state.period : 'all';
  const win = timed ? windowOf(period, now, rows[0]?.at) : null;
  // Without a series (an older Lazybox), all time from the groups, and no chart.
  const view = timed ? summarize(rows, win) : summarize(usage.groups.map(g => ({ ...g, at: 0, unpriced: 0 })), null);
  state.view = { ...view, win, period };

  renderPeriod(timed);
  renderHero(view, win, period, rows);
  renderFilters();
  renderTiles(view.total, view.conversations);
  $('chart').closest('.card').hidden = !timed;
  if (timed) renderChart();
  renderRank($('by-model'), 'model', view.models.map(m => ({
    key: m.key, label: modelName(m.key), sub: m.provider, usd: m.usd, slot: slotOf(m.key), detail: m,
    pressed: state.filter.model === m.key, action: () => toggleFilter('model', m.key),
  })), view.total.usd, 6, 'Nothing spent on a model in this period');
  renderRank($('by-workspace'), 'workspace', view.workspaces.map(w => ({
    key: w.key, label: w.name || 'Workspace', sub: plural(w.conversations.size, 'conversation', 'conversations'), usd: w.usd, detail: w,
    pressed: state.filter.workspace === w.key, action: () => toggleFilter('workspace', w.key),
  })), view.total.usd, 6, 'Nothing spent in a workspace in this period');
  renderRank($('by-conversation'), 'conversation', view.conversations.map(c => {
    const known = state.conversations.get(c.key);
    return {
      key: c.key, label: known?.title || 'Untitled conversation', sub: [c.workspaceName, [...c.models].map(modelName).join(', ')].filter(Boolean).join(' · '), usd: c.usd, detail: c,
      action: state.canShow && known?.sessionId ? () => show(known.sessionId) : null, actionLabel: 'Show',
    };
  }), view.total.usd, 8, 'No conversation spent anything in this period');
}

function renderPeriod(timed) {
  for (const button of $('period').querySelectorAll('button')) {
    const on = (timed ? state.period : 'all') === button.dataset.period;
    button.setAttribute('aria-checked', String(on));
    button.tabIndex = on ? 0 : -1;
    button.disabled = !timed && button.dataset.period !== 'all';
  }
}

function renderHero(view, win, period, rows) {
  const scope = [state.filter.workspace && workspaceName(state.filter.workspace), state.filter.model && modelName(state.filter.model)].filter(Boolean);
  $('hero-label').textContent = PERIODS[period].title + (scope.length ? ' · ' + scope.join(' · ') : '');
  $('total').textContent = dollars(view.total.usd);
  const delta = $('delta');
  delta.hidden = true; delta.className = 'delta'; delta.replaceChildren();
  if (win?.prev) {
    const before = sumBetween(rows, win.prev[0], win.prev[1]), now = view.total.usd;
    if (before > 0.005) {
      const change = (now - before) / before;
      const level = Math.abs(change) < 0.005;
      delta.classList.add(level ? 'level' : change > 0 ? 'up' : 'down');
      delta.append(el('b', '', level ? '= level' : `${change > 0 ? '▲' : '▼'} ${Math.round(Math.abs(change) * 100).toLocaleString()}%`), ` vs ${dollars(before)} ${PERIODS[period].compare}`);
      delta.hidden = false;
    } else if (now > 0) {
      delta.append(`Nothing spent ${PERIODS[period].compare}`);
      delta.hidden = false;
    }
  }
  const t = view.total, notes = [];
  if (win && win.unit !== 'hour' && t.usd > 0) {
    const days = Math.max(1, Math.round((Math.min(win.end, Date.now()) - win.start) / 86400000));
    notes.push(`${dollars(t.usd / days)} a day on average`);
  }
  notes.push(plural(t.records, 'turn', 'turns'));
  if (t.unpriced) notes.push(`${whole.format(t.unpriced)} unpriced`);
  $('hero-note').textContent = notes.join(' · ');
}

function renderFilters() {
  const box = $('filters');
  const chips = [];
  if (state.filter.workspace) chips.push(chip(`Workspace: ${workspaceName(state.filter.workspace)}`, () => toggleFilter('workspace', state.filter.workspace)));
  if (state.filter.model) chips.push(chip(`Model: ${modelName(state.filter.model)}`, () => toggleFilter('model', state.filter.model)));
  box.replaceChildren(...chips);
  box.hidden = !chips.length;
}
function chip(text, clear) {
  const node = el('span', 'chip');
  const button = el('button', '', '×');
  button.type = 'button';
  button.setAttribute('aria-label', `Clear ${text}`);
  button.addEventListener('click', clear);
  node.append(el('span', '', text), button);
  return node;
}

function renderTiles(t, conversations) {
  const input = t.input + t.cacheRead + t.cacheWrite;
  $('tokens-in').textContent = tokens(input);
  $('tokens-in-note').textContent = input ? `${tokens(t.input + t.cacheWrite)} uncached · ${tokens(t.cacheRead)} from cache` : 'No input in this period';
  $('tokens-out').textContent = tokens(t.output);
  $('tokens-out-note').textContent = t.records ? `${plural(t.records, 'turn', 'turns')} · ${tokens(Math.round(t.output / t.records))} a turn` : 'No turns in this period';
  $('cache-rate').textContent = input ? percent(t.cacheRead, input) : '–';
  $('cache-meter').style.width = input ? `${(t.cacheRead / input * 100).toFixed(1)}%` : '0';
  $('cache-note').textContent = input ? `${tokens(t.cacheRead)} of ${tokens(input)} input read from cache` : 'Nothing read yet';

  const top = conversations[0], tile = $('top-tile');
  const known = top && state.conversations.get(top.key);
  $('top-cost').textContent = top ? dollars(top.usd) : '–';
  $('top-title').textContent = top ? (known?.title || 'Untitled conversation') : 'No conversation in this period';
  $('top-note').textContent = top ? [`${percent(top.usd, t.usd)} of spend`, top.workspaceName].filter(Boolean).join(' · ') : '';
  const actionable = !!(state.canShow && known?.sessionId);
  tile.toggleAttribute('data-action', actionable);
  tile.tabIndex = actionable ? 0 : -1;
  if (actionable) { tile.setAttribute('role', 'button'); tile.setAttribute('aria-label', `Show ${known.title}, ${dollars(top.usd)}, ${percent(top.usd, t.usd)} of spend`); }
  else { tile.removeAttribute('role'); tile.removeAttribute('aria-label'); }
  tile.title = top ? `${percent(top.usd, t.usd)} of the period's spend${actionable ? ' — show it' : ''}` : '';
  tile.onclick = actionable ? () => show(known.sessionId) : null;
  tile.onkeydown = actionable ? e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); show(known.sessionId); } } : null;
}

function renderRank(list, kind, rows, total, limit, none) {
  if (!rows.length) { list.replaceChildren(el('li', 'none', none)); return; }
  const open = state.expanded.has(kind);
  const top = Math.max(...rows.map(r => r.usd), 0);
  const items = (open ? rows : rows.slice(0, limit)).map(row => {
    const li = el('li');
    const node = el(row.action ? 'button' : 'div', 'row');
    const d = row.detail;
    const detail = `${tokens(d.input + d.cacheRead + d.cacheWrite)} in · ${tokens(d.output)} out · ${plural(d.records, 'turn', 'turns')}${d.unpriced ? ` · ${d.unpriced} without a price` : ''}`;
    node.title = `${row.label}: ${dollars(row.usd)} · ${detail}`;
    if (row.action) {
      node.type = 'button';
      node.addEventListener('click', row.action);
      if (row.pressed !== undefined) node.setAttribute('aria-pressed', String(row.pressed));
    }
    const who = el('span', 'who'), label = el('span', 'label');
    if (kind === 'model') who.append(el('span', `swatch c${row.slot}`));
    label.append(el('span', 'name', row.label));
    if (row.sub) label.append(el('span', 'sub', row.sub));
    who.append(label);
    const track = el('span', 'track'), fill = el('span', kind === 'model' ? `fill c${row.slot}` : 'fill');
    fill.style.width = (top > 0 ? Math.max(0.5, row.usd / top * 100) : 0) + '%';
    track.append(fill);
    // What has no price says so, rather than $0.00.
    const unpriced = !row.usd && d.unpriced > 0;
    node.append(who, el('span', unpriced ? 'cost unpriced' : 'cost', unpriced ? 'unpriced' : dollars(row.usd)), el('span', 'share', unpriced ? '' : percent(row.usd, total)), track);
    li.append(node);
    return li;
  });
  if (rows.length > limit) {
    const li = el('li');
    const more = el('button', 'more', open ? 'Show fewer' : `Show all ${rows.length}`);
    more.type = 'button';
    more.setAttribute('aria-expanded', String(open));
    more.addEventListener('click', () => { if (open) state.expanded.delete(kind); else state.expanded.add(kind); render(); list.querySelector('.more')?.focus(); });
    li.append(more);
    items.push(li);
  }
  list.replaceChildren(...items);
}

// ---- Spend over time: stacked columns, one per hour, day or week.
function renderChart() {
  const { bins, win, total } = state.view;
  const unit = win.unit;
  $('spend-title').textContent = unit === 'hour' ? 'Spend per hour' : unit === 'week' ? 'Spend per week' : 'Spend per day';
  const present = new Set();
  for (const bin of bins) for (const [slot, v] of bin) if (v > 0) present.add(slot);
  const legend = [];
  for (const [model, slot] of state.slots) if (slot && present.has(slot) && (!state.filter.model || state.filter.model === model)) legend.push([slot, modelName(model)]);
  legend.sort((a, b) => a[0] - b[0]);
  if (present.has(0)) legend.push([0, 'Other models']);
  $('legend').replaceChildren(...legend.map(([slot, name]) => { const li = el('li'); li.append(el('span', `swatch c${slot}`), el('span', '', name)); return li; }));
  $('legend').hidden = legend.length < 2;

  const sums = bins.map(bin => [...bin.values()].reduce((a, b) => a + b, 0));
  const peak = Math.max(...sums, 0), peakAt = sums.indexOf(peak);
  const busiest = peak > 0 ? `${unit === 'hour' ? 'at' : unit === 'week' ? 'in the' : 'on'} ${unit === 'week' ? win.bins[peakAt].long.toLowerCase() : win.bins[peakAt].long}` : '';
  $('chart-hint').textContent = peak > 0 ? `Highest: ${dollars(peak)} ${busiest}` : '';
  $('chart').setAttribute('aria-label', `${$('spend-title').textContent}, ${PERIODS[state.view.period].label}: ${dollars(total.usd)} in all${peak > 0 ? `; highest ${dollars(peak)}, ${busiest}` : ''}. Use the arrow keys to read each ${unit}.`);
  if (state.active >= bins.length) state.active = -1;
  drawChart(sums);
}

function niceStep(max, count) {
  const raw = max / count, mag = 10 ** Math.floor(Math.log10(raw)), n = raw / mag;
  return (n <= 1 ? 1 : n <= 2 ? 2 : n <= 2.5 ? 2.5 : n <= 5 ? 5 : 10) * mag;
}

function drawChart(sums) {
  const box = $('chart'), root = $('chart-svg');
  const width = box.clientWidth, height = box.clientHeight;
  if (!width || !height) return;
  const { bins, win } = state.view;
  root.replaceChildren();
  root.setAttribute('viewBox', `0 0 ${width} ${height}`);
  const peak = Math.max(...sums, 0);
  const step = peak > 0 ? niceStep(peak, height > 200 ? 4 : 3) : 1;
  const top = peak > 0 ? Math.ceil(peak / step) * step : 1;
  const ticks = [];
  for (let v = 0; v <= top + step / 2; v += step) ticks.push(v);
  const left = Math.max(...ticks.map(v => axisDollars(v, step).length)) * 6.2 + 8;
  const plot = { x: left, y: 8, w: width - left - 2, h: height - 8 - 20 };
  const y = v => plot.y + plot.h - v / top * plot.h;

  const grid = svg('g', {}, root);
  for (const v of ticks) {
    svg('line', { class: v === 0 ? 'grid base' : 'grid', x1: plot.x, x2: plot.x + plot.w, y1: Math.round(y(v)) + 0.5, y2: Math.round(y(v)) + 0.5 }, grid);
    svg('text', { x: plot.x - 8, y: y(v) + 3.5, 'text-anchor': 'end' }, grid).textContent = peak > 0 || v === 0 ? axisDollars(v, step) : '';
  }

  const band = plot.w / bins.length;
  const barW = Math.max(1, Math.min(24, band > 6 ? band * 0.62 : band - 1));
  const labelEvery = Math.max(1, Math.ceil(bins.length / Math.max(2, Math.floor(plot.w / (win.unit === 'hour' ? 44 : 56)))));
  const cols = svg('g', {}, root);
  bins.forEach((bin, i) => {
    const col = svg('g', { class: 'col' + (i === state.active ? ' on' : ''), 'data-i': i }, cols);
    svg('rect', { class: 'hit', x: plot.x + i * band, y: plot.y, width: band, height: plot.h, rx: 4 }, col);
    const x = plot.x + i * band + (band - barW) / 2;
    const parts = [...bin.entries()].filter(([, v]) => v > 0).sort((a, b) => (a[0] || 99) - (b[0] || 99));
    let base = 0;
    parts.forEach(([slot, v], j) => {
      const y0 = y(base), y1 = y(base + v);
      base += v;
      const gap = j > 0 && y0 - y1 > 3 ? 2 : 0;
      const yb = y0 - gap, h = yb - y1;
      if (h <= 0.2) return;
      const last = j === parts.length - 1;
      const r = last ? Math.min(4, barW / 2, h) : 0;
      svg('path', { class: `seg c${slot}`, d: r ? `M${x},${yb}V${y1 + r}Q${x},${y1} ${x + r},${y1}H${x + barW - r}Q${x + barW},${y1} ${x + barW},${y1 + r}V${yb}Z` : `M${x},${yb}V${y1}H${x + barW}V${yb}Z` }, col);
    });
    if ((bins.length - 1 - i) % labelEvery === 0) {
      svg('text', { x: plot.x + i * band + band / 2, y: plot.y + plot.h + 14, 'text-anchor': 'middle' }, root).textContent = win.bins[i].label;
    }
  });
  if (peak === 0) svg('text', { x: plot.x + plot.w / 2, y: plot.y + plot.h / 2, 'text-anchor': 'middle', class: 'nothing' }, root).textContent = `Nothing spent ${PERIODS[state.view.period].label}`;
  state.chart = { plot, band, sums };
  showTip();
}

function showTip() {
  const tip = $('tooltip'), i = state.active;
  for (const col of $('chart-svg').querySelectorAll('.col')) col.classList.toggle('on', Number(col.dataset.i) === i);
  if (i < 0 || !state.chart || !state.view?.win) { tip.hidden = true; return; }
  const { plot, band, sums } = state.chart, bin = state.view.win.bins[i], parts = state.view.bins[i];
  const names = new Map();
  for (const [model, slot] of state.slots) if (slot && !names.has(slot)) names.set(slot, modelName(model));
  const list = el('ul');
  for (const [slot, v] of [...parts.entries()].filter(([, v]) => v > 0).sort((a, b) => b[1] - a[1])) {
    const li = el('li');
    li.append(el('span', `key c${slot}`), el('span', 'name', slot ? names.get(slot) : 'Other models'), el('span', 'val', dollars(v)));
    list.append(li);
  }
  tip.replaceChildren(el('div', 'when', bin.long), el('div', 'sum', sums[i] > 0 ? dollars(sums[i]) : 'Nothing spent'));
  if (list.children.length > 1 || (list.children.length === 1 && state.slots.size > 1)) tip.append(list);
  tip.hidden = false;
  const box = $('chart'), w = tip.offsetWidth, center = plot.x + i * band + band / 2;
  let left = center + band / 2 + 8;
  if (left + w > box.clientWidth) left = center - band / 2 - 8 - w;
  tip.style.left = `${Math.max(0, left)}px`;
  tip.style.top = `${plot.y}px`;
}

function setActive(i) { state.active = i; showTip(); }

// ---- Acting.
function toggleFilter(kind, key) {
  state.filter[kind] = state.filter[kind] === key ? null : key;
  state.active = -1;
  render();
}
function show(sessionId) {
  lazybox.conversations.show(sessionId).catch(error => say(error.code === 'not-granted' ? 'This dashboard may not show conversations; allow it in Settings → Extensions.' : error.message));
}
function choosePeriod(period, focus) {
  if (state.period === period) return;
  state.period = period;
  state.active = -1;
  render();
  if (focus) $('period').querySelector(`[data-period="${period}"]`).focus();
}

$('period').addEventListener('click', e => { const b = e.target.closest('button[data-period]'); if (b && !b.disabled) choosePeriod(b.dataset.period); });
$('period').addEventListener('keydown', e => {
  const buttons = [...$('period').querySelectorAll('button:not(:disabled)')];
  const at = buttons.indexOf(document.activeElement);
  const to = { ArrowRight: at + 1, ArrowDown: at + 1, ArrowLeft: at - 1, ArrowUp: at - 1, Home: 0, End: buttons.length - 1 }[e.key];
  if (to === undefined || at < 0) return;
  e.preventDefault();
  choosePeriod(buttons[(to + buttons.length) % buttons.length].dataset.period, true);
});

const chart = $('chart');
chart.addEventListener('pointermove', e => {
  if (!state.chart) return;
  const r = chart.getBoundingClientRect(), x = e.clientX - r.left - state.chart.plot.x;
  const i = Math.floor(x / state.chart.band);
  setActive(i >= 0 && i < state.view.win.bins.length ? i : -1);
});
chart.addEventListener('pointerleave', () => setActive(-1));
chart.addEventListener('blur', () => setActive(-1));
chart.addEventListener('focus', () => {
  if (state.active >= 0 || !state.chart) return;
  const sums = state.chart.sums;
  let last = sums.length - 1;
  while (last > 0 && !sums[last]) last--;
  setActive(last);
});
chart.addEventListener('keydown', e => {
  if (!state.chart) return;
  const n = state.chart.sums.length;
  const to = { ArrowRight: state.active + 1, ArrowLeft: state.active - 1, Home: 0, End: n - 1 }[e.key];
  if (e.key === 'Escape') { setActive(-1); return; }
  if (to === undefined) return;
  e.preventDefault();
  setActive(Math.max(0, Math.min(n - 1, to)));
});
new ResizeObserver(() => { if (state.view?.win) drawChart(state.chart?.sums || state.view.bins.map(b => [...b.values()].reduce((a, c) => a + c, 0))); }).observe(chart);

// ---- Reading.
async function refresh() {
  try {
    const info = await lazybox.ready;
    state.canShow = info.grants.includes('show:conversations');
    const [usage, sessions] = await Promise.all([
      lazybox.usage.read({ series: true }),
      lazybox.sessions.list({ limit: 500 }).catch(() => ({ conversations: [] })),
    ]);
    state.conversations = new Map(sessions.conversations.map(c => [c.conversationId, { sessionId: c.sessionId, title: c.title, workspaceName: c.workspaceName }]));
    state.usage = usage;
    assignSlots(usage.series || usage.groups);
    render();
    if (!hasSeries(usage)) say('This Lazybox does not report usage over time yet: the totals are for all time.');
    else say('');
    $('updated').textContent = 'Updated ' + new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  } catch (error) {
    if (error.code === 'not-granted') {
      setState('empty', 'Usage is not allowed', 'This dashboard reads token use and cost. Allow “See token use and cost” in Settings → Extensions.');
    } else if (!state.usage) {
      setState('empty', 'Usage could not be read', error.message || String(error));
    } else {
      say(`Could not refresh: ${error.message || error}`);
    }
  }
}

lazybox.every(30000, refresh);
