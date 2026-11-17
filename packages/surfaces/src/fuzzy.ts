/**
 * Fuzzy matching for the command palette: every character of the query must
 * appear in order. Matches score higher at the start of the text and of
 * words, when consecutive, and in shorter texts — so "ncon" ranks "New
 * conversation" above "Next conversation that needs you" only if it is a
 * better fit, and an exact prefix always wins.
 */
export type FuzzyMatch = { score: number; positions: number[] };

const boundary = (text: string, i: number) => i === 0 || /[\s/_\-.:·]/.test(text[i - 1]) || (/[a-z]/.test(text[i - 1]) && /[A-Z]/.test(text[i]));

export function fuzzyMatch(query: string, text: string): FuzzyMatch | null {
  const q = query.toLowerCase().replace(/\s+/g, ' ').trim();
  if (!q) return { score: 0, positions: [] };
  const lower = text.toLowerCase();
  // An exact substring is the strongest signal; a prefix or word start even more so.
  const at = lower.indexOf(q);
  if (at >= 0) {
    const score = 1000 + (at === 0 ? 400 : boundary(text, at) ? 200 : 0) - at - text.length * 0.5;
    return { score, positions: Array.from({ length: q.length }, (_, i) => at + i) };
  }
  const needle = q.replace(/ /g, '');
  // Greedy left-to-right, preferring word starts: look ahead for a boundary match first.
  const positions: number[] = [];
  let from = 0, score = 0, previous = -2;
  for (const char of needle) {
    let found = -1;
    for (let i = from; i < lower.length; i++) if (lower[i] === char && boundary(text, i)) { found = i; break; }
    // A word start far away is worse than the next occurrence right after the last match.
    const next = lower.indexOf(char, from);
    if (next < 0) return null;
    if (found < 0 || (next === previous + 1)) found = next;
    positions.push(found);
    score += 10;
    if (found === previous + 1) score += 15;
    if (boundary(text, found)) score += 20;
    if (found === 0) score += 15;
    score -= Math.min(found - from, 10);
    previous = found; from = found + 1;
  }
  return { score: score - text.length * 0.5, positions };
}

/** Best of several fields: the title counts fully, other text (keywords, workspace) less. */
export function bestMatch(query: string, title: string, extra = ''): FuzzyMatch | null {
  const main = fuzzyMatch(query, title);
  const other = extra ? fuzzyMatch(query, extra) : null;
  if (main && (!other || main.score >= other.score * 0.6)) return main;
  return other ? { score: other.score * 0.6, positions: [] } : null;
}
