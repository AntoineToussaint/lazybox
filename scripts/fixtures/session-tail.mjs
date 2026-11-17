// ReadSessionTail as the Runner pages a conversation's timeline
// (services/runner/code/internal/runner/timeline.go), for the fixture Runner
// and the reader's node tests: about maxBytes of records ending before
// `before`, starting where one of the person's messages starts a turn when
// `turns` is set, and at least one record.

const lazyboxOperation = /^(repository-note-|review-brief-|review-ask-|review-answer-)|^review-.+-(findings|round)-\d+$/;
const decode = data => {
  if (!data || !data.length) return {};
  try { return JSON.parse(typeof data === 'string' ? Buffer.from(data, 'base64').toString() : new TextDecoder().decode(data)); } catch { return {}; }
};

/** A message the person sent (not Lazybox's, not an answer, not a sub-agent's): a turn starts there. */
export function turnStart(event) {
  if (event?.type !== 'operation_accepted' || !event.text || event.requestId || (event.agentId && event.agentId !== 'root')) return false;
  return !decode(event.providerDataJson)?.origin && !lazyboxOperation.test(event.operationId || '');
}

/**
 * The page of `records` (ascending, cursors 1…n, `record.event` the agent
 * event) that ends just before `before` (0: at the last). Undefined when
 * `before` is past the end, like the Runner's OutOfRange.
 */
export function tailPage(records, { before = 0, turns = 0, maxBytes = 0 } = {}, size = record => JSON.stringify(record).length + 1) {
  const retained = records.length, end = Number(before || 0) > 0 ? Number(before) - 1 : retained;
  if (end > retained) return undefined;
  if (!end) return { events: [], start: 1, retained };
  const budget = Number(maxBytes) || 1 << 20;
  let start = end, used = size(records[end - 1]);
  while (start > 1 && used + size(records[start - 2]) <= budget) { start--; used += size(records[start - 1]); }
  if (Number(turns) > 0) {
    const marks = [];
    for (let at = 1; at <= end; at++) if (turnStart(records[at - 1].event)) marks.push(at);
    const aligned = marks.length >= turns ? marks[marks.length - turns] : 1;
    if (aligned >= start) start = aligned;
    else { const fitting = marks.find(mark => mark >= start); if (fitting) start = fitting; }
  }
  return { events: records.slice(start - 1, end), start, retained };
}
