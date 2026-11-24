/**
 * The agent reads a page as Playwright's accessibility snapshot in "ai" mode:
 * a compact YAML outline of roles and names where every element it can act on
 * carries a ref ([ref=e12]). It is far smaller than HTML and stable enough to
 * act on, and refs resolve with page.getByRef until the next snapshot.
 */

/** Snapshot text returned when the caller does not say. */
export const defaultMaxChars = 8000;
/** No caller gets more than this in one call. */
export const maxMaxChars = 60000;

/** Caps a snapshot, saying what was left out and how to see it. */
export function capSnapshot(text: string, maxChars = defaultMaxChars): string {
  const limit = Math.min(Math.max(Math.floor(maxChars) || defaultMaxChars, 500), maxMaxChars);
  if (text.length <= limit) return text;
  // Cut at a line so no ref is split.
  const cut = text.lastIndexOf('\n', limit);
  const kept = text.slice(0, cut > 0 ? cut : limit);
  return `${kept}\n(truncated — ${text.length - kept.length} more characters; scroll or use browser_snapshot with a larger maxChars)`;
}

/**
 * What each ref of a snapshot names, for the action log and the agent's
 * result: `button "Sign in"`, `textbox "Email"`, `link "Docs"`.
 */
export function refLabels(snapshot: string): Map<string, string> {
  const labels = new Map<string, string>();
  for (const line of snapshot.split('\n')) {
    const match = /^\s*- (.*?)\s*\[ref=([\w]+)\]/.exec(line);
    if (!match) continue;
    // Keep the role and its quoted name; drop state flags such as [level=1] or [active].
    const label = match[1].replace(/\s*\[[^\]]*\]/g, '').trim();
    labels.set(match[2], label || 'element');
  }
  return labels;
}

/**
 * A text field's line with its value: `- textbox "Password" [ref=e6]: hunter2`
 * (the role, its quoted name, its flags and ref, then the value).
 */
const filledTextbox = /^(\s*- textbox(?: "(?:[^"\\]|\\.)*")?((?: \[[^\]]*\])*)):\s.*$/;

/** The text fields of a snapshot that show a value, by line and ref. */
export function filledTextboxes(snapshot: string): { line: number; ref: string }[] {
  const fields: { line: number; ref: string }[] = [];
  snapshot.split('\n').forEach((text, line) => {
    const match = filledTextbox.exec(text);
    const ref = match && /\[ref=(\w+)\]/.exec(match[2])?.[1];
    if (ref) fields.push({ line, ref });
  });
  return fields;
}

/** A text field's line without its value: the agent sees that it is filled, never with what. */
export function hiddenValue(line: string): string {
  const match = filledTextbox.exec(line);
  return match ? `${match[1]}: "(password hidden)"` : line;
}

/** The header of every page-reading result. */
export function pageLine(title: string, url: string): string {
  return title ? `"${title}" — ${url}` : url;
}

/** Text quoted in a summary, shortened. */
export function quote(text: string, limit = 60): string {
  const flat = text.replace(/\s+/g, ' ').trim();
  return `"${flat.length > limit ? flat.slice(0, limit - 1) + '…' : flat}"`;
}
