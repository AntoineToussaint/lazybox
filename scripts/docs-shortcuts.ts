// The keyboard shortcut reference (docs/reference/keyboard-shortcuts.md),
// rendered from the action registry (packages/surfaces/src/actions.ts) so the
// page can never drift from the keys the app binds.
//
//   npm run docs:shortcuts        regenerate the page
//   npm run test:client           fails when the committed page is stale
//
// The registry section is generated. The "Keys inside a panel" section below
// is written here by hand: those keys belong to one component (the Changes
// diff, an approval card, the inbox) rather than to the registry. Keep it true
// to the components it names.
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { actionDefinitions, actionGroups, bindings, defaultKeys, displayBinding, type ActionDefinition } from '../packages/surfaces/src/actions';

export const shortcutsPage = fileURLToPath(new URL('../docs/reference/keyboard-shortcuts.md', import.meta.url));

/** One binding as it reads on screen: `⇧⌘P`, `]] i`, `Ctrl+K`. */
function keyText(binding: string, mac: boolean): string {
  const keys = displayBinding(binding, mac);
  const text = keys.join(' ').replace(/^\] \](?= |$)/, ']]');
  return '`' + text + '`';
}
function keysCell(definition: ActionDefinition, mac: boolean): string {
  const resolved = bindings(defaultKeys(definition, mac));
  return resolved.length ? resolved.map(binding => keyText(binding, mac)).join(' or ') : '—';
}

const groupIntro: Record<string, string> = {
  'General': 'Work from anywhere in the window, including while a dialog is open for the palette and the shortcuts overlay.',
  'Go to': 'Move between conversations and workspaces. Chords with ⌘ (Ctrl) work even while you type in the message box.',
  'Workspace': 'Open the parts of the workspace in front.',
  'Layout': [
    'Act on the active panel group: the one you last clicked or focused, ringed while you use these keys.',
    'With the arrows, `⌥⌘` moves the focus to the neighbouring group; `⌥⇧⌘` moves the active panel into it, or swaps the two when the panel is alone in its group (with no group that way, a tab splits out to that side); `⌃⌘` moves the group\'s edge that way, 5 % a press and faster held down (at the sidebar, its one width changes).',
    '`⌃1`–`⌃8` bring a tab of the group to the front and `⌃9` the last one; `⌘1`–`⌘9` stay *Go to workspace*. The **+** after a group\'s tabs (or `⌘T`) starts a conversation there or opens any panel there.',
    'While a panel is maximized, moving the focus, `⌘J`, `⌘]` and opening a conversation keep it maximized and show the new one.',
    'Two exceptions keep code-editor habits: in the file editor `⌥⌘←` / `⌥⌘→` (with `⇧`, to move) are its own tabs, and in code `⌥⌘↑` / `⌥⌘↓` add a cursor.',
    'On Windows and Linux, where Ctrl stands for both ⌘ and ⌃, the keys differ: see the right-hand column.',
  ].join(' '),
  'Conversation': 'Act on the conversation you last clicked or focused.',
  'Voice': 'Hold the key and speak; release to finish (a quick tap keeps listening until you press it again). `Right ⌥` is the Option key on the right, alone: hold it to talk, tap it twice to keep listening, tap it once to stop; with any other key it stays an ordinary ⌥. While dictating, `⌘↵` sends once the last words are in. See [Voice](../guides/voice.md).',
  'Sidebar': 'After `]]` puts the keyboard in the sidebar. The *Arrows* preset uses the arrow keys instead.',
  'Message box': 'While you type a message. While the agent works, `↵` queues your message and `⌘↵` stops the agent\'s current step and sends it now; with an empty box, `⌘↵` sends the oldest queued message now. While the agent is idle, `⌘↵` sends like `↵`, also while you dictate.',
};

export function renderShortcuts(): string {
  const lines: string[] = [];
  const push = (...more: string[]) => lines.push(...more);
  push(
    '# Keyboard shortcuts',
    '',
    '<!-- GENERATED FILE — do not edit by hand.',
    '     Regenerate with: npm run docs:shortcuts (from the repository root)',
    '     Source: packages/surfaces/src/actions.ts, rendered by scripts/docs-shortcuts.ts. -->',
    '',
    'Every command in Lazybox is declared once, in the action registry. The keys',
    'below are its **defaults**; the in-app overlay (`?`) always shows *your* keys.',
    '',
    '- **Change a key:** Settings → Shortcuts. Pick a preset — **Vim** (`]]` and',
    '  `h j k l`, the default), **Ctrl+b** (a tmux-style prefix with arrows) or',
    '  **Arrows** — then edit any binding. The Ctrl+b preset replaces every `]]`',
    '  below with `Ctrl+b`.',
    '- **Or edit the file.** Your keys are',
    '  [`keys.yaml`](configuration.md#keysyaml-your-shortcuts) beside your actions',
    '  file: Settings writes it (keeping your comments), and a save in your editor',
    '  applies within two seconds. `lazybox do --list` prints every action id.',
    '- **Forgot a key?** `⌘K` opens the command palette: every action is there by',
    '  name, with its key beside it. An action with no key (—) runs from the palette.',
    '- **`]]` is the leader.** From the message box, `]]` leaves typing and shows a',
    '  menu of every next key; a single `]` is still text. Settings → Shortcuts can',
    '  add your own entries to that menu (one key sends or inserts a snippet), and',
    '  [your actions](configuration.md#actionsyaml-your-actions-and-hooks) can take a',
    '  key there too.',
    '- On Windows and Linux, read `Ctrl` for `⌘` (the tables give both).',
    '- **Lost?** `⌃⌥⇧Esc` (Ctrl+Alt+Shift+Esc) puts back the default layout *and*',
    '  the default keys in one step, with **Undo** in the corner. It is the one',
    '  shortcut you cannot change, so it always works.',
    '',
    '![The shortcuts overlay (?): every action with your keys](../images/shortcuts.webp)',
    '',
  );
  for (const group of actionGroups) {
    const members = actionDefinitions.filter(definition => definition.group === group);
    if (!members.length) continue;
    push(`## ${group}`, '', groupIntro[group] || '', '', '| Action | macOS | Windows / Linux |', '| --- | --- | --- |');
    const seenSeries = new Set<string>();
    for (const definition of members as readonly ActionDefinition[]) {
      if (definition.series) {
        if (seenSeries.has(definition.series)) continue;
        seenSeries.add(definition.series);
        const series = members.filter(other => (other as ActionDefinition).series === definition.series) as ActionDefinition[];
        const first = series[0], last = series[series.length - 1];
        const title = first.title.replace(/\d+$/, `1…${series.length}`);
        const range = (mac: boolean) => `${keysCell(first, mac)} … ${keysCell(last, mac)}`;
        push(`| ${title} | ${range(true)} | ${range(false)} |`);
        continue;
      }
      const fixed = definition.fixed ? ' (cannot be changed)' : '';
      push(`| ${definition.title} | ${keysCell(definition, true)}${fixed} | ${keysCell(definition, false)}${fixed} |`);
    }
    push('');
  }
  push(
    '## Keys inside a panel',
    '',
    'These work while the panel or card has focus, never while you type in a text field.',
    '',
    '| Where | Keys |',
    '| --- | --- |',
    '| Message box | `Enter` sends · `Shift+Enter` new line · `↑` in an empty box recalls your sent messages, `↓` walks back · `⌥↑` / `⌥↓` jump between your prompts in the conversation |',
    '| Approval card | `y` allow once · `a` always allow (at the first scope offered) · `n` deny, with an optional reason · `1`–`9` pick a row |',
    '| Question card | `1`–`9`, `↑` `↓`, `Space` and `Enter` pick options · `⌘↵` sends the answers |',
    '| Sub-agent card, chip or Background work row | `Delete` or `⌫` stops a running sub-agent or background command (the main agent continues) · `Space` opens a card in a floating panel · `Esc` closes its steps |',
    '| Changes | `n` / `p` next / previous hunk · `Shift+N` / `Shift+P` next / previous file · `v` marks the file viewed and moves on · `s` switches unified / split |',
    '| Review comment | `⌘↵` saves · `Esc` cancels · `Tab` to a line\'s **+** then `Enter` starts a comment (`Shift+Enter` extends the range) |',
    '| Agent review card | `Esc` cancels the review under way (it asks first when the reviewer has filed findings; they are kept) |',
    '| Inbox | `j` / `k` move · `Enter` opens · `m` marks seen |',
    '| Any row you can peek | `⌥`-click or `Space` peeks it · in the peek `←` / `→` step through rows, `⌘↵` opens it, `Space` or `Esc` closes |',
    '| Sidebar workspace row | `F2` renames · `⌥↓` opens its card · `⌥⇧↑` / `⌥⇧↓` move it up or down (or drag it; `Esc` puts it back) |',
    '| File editor | `⌥Z` toggles word wrap · with Vim editing on: `:w`, `:q`, `:wq` |',
    '| Desktop window | `⌘=` / `⌘-` / `⌘0` zoom in, out, reset |',
    '',
  );
  return lines.join('\n');
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  writeFileSync(shortcutsPage, renderShortcuts());
  console.log(`Wrote ${shortcutsPage}`);
}
