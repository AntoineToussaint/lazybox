// An explicitly fake set of config files (POWER.md, "Config files") for the
// browser fixture: keys.yaml, settings.yaml and layouts/*.yaml as the Runner
// would hand them to the app. The Runner's own parser and comment-keeping
// writer are tested in Go (internal/power); here a test sets what the Runner
// would have loaded (`setFile`), and the app's edits are applied to that
// value the way the Runner's would be, and recorded, so the UI round trip is
// exercised. Layout YAML is read by a small parser for the subset the app
// writes (block mappings and lists, [a, b] lists, quoted strings).
const now = () => String(Date.now());
const DIR = '/fixture/config/lazybox';
const PANELS = ['collections', 'agents', 'chat', 'console', 'terminal', 'assistant', 'changes', 'usage', 'sessions', 'missions', 'pulls', 'processes', 'actions', 'preview', 'web', 'browser', 'files', 'editor', 'artifacts'];
const NOTIFICATIONS = { 'needs-input': 'needsInput', failed: 'failed', review: 'review', 'dock-badge': 'badge', 'sound-needs-you': 'soundNeedsYou', 'sound-done': 'soundDone' };
// settings.yaml's appearance names → the app's (internal/power/settings.go, AppearanceFields).
const APPEARANCE = { mode: 'mode', 'dark-theme': 'darkTheme', 'light-theme': 'lightTheme', accent: 'accent', 'min-contrast': 'minContrast', motion: 'motion', density: 'density', 'interface-font': 'uiFont', 'interface-font-size': 'uiFontSize', 'code-font': 'codeFont', 'code-font-size': 'codeFontSize', ligatures: 'ligatures', 'chat-font-size': 'chatFontSize', 'chat-line-height': 'chatLineHeight', 'chat-paragraph-gap': 'chatParagraphGap', 'chat-line-width': 'chatMaxWidth', zoom: 'zoom' };

/** The subset of YAML the app writes for a layout: returns [value, lineOf(text)]. */
export function parseLayoutYaml(text) {
  const lines = text.split('\n').map((raw, i) => ({ raw: raw.replace(/\s+#.*$/, '').replace(/^#.*$/, ''), n: i + 1 })).filter(l => l.raw.trim());
  let at = 0;
  const indentOf = l => l.raw.length - l.raw.trimStart().length;
  const scalar = value => {
    value = value.trim();
    if (value.startsWith('[') && value.endsWith(']')) return value.slice(1, -1).split(',').map(v => v.trim()).filter(Boolean).map(scalar);
    if (/^".*"$/.test(value)) return JSON.parse(value);
    if (/^'.*'$/.test(value)) return value.slice(1, -1).replace(/''/g, "'");
    if (/^-?\d+(\.\d+)?$/.test(value)) return Number(value);
    if (value === 'true' || value === 'false') return value === 'true';
    return value;
  };
  const block = indent => {
    const first = lines[at];
    if (!first || indentOf(first) < indent) return null;
    if (first.raw.trimStart().startsWith('- ')) {
      const list = [];
      while (lines[at] && indentOf(lines[at]) === indent && lines[at].raw.trimStart().startsWith('- ')) {
        const line = lines[at];
        // "- key: value" opens a mapping whose other keys align after the dash.
        line.raw = ' '.repeat(indent + 2) + line.raw.trimStart().slice(2);
        list.push(/^[\w-]+:/.test(line.raw.trim()) ? block(indent + 2) : (at++, scalar(line.raw)));
      }
      return list;
    }
    const map = {};
    while (lines[at] && indentOf(lines[at]) === indent && !lines[at].raw.trimStart().startsWith('- ')) {
      const line = lines[at++], match = /^([\w-]+):\s*(.*)$/.exec(line.raw.trim());
      if (!match) throw Object.assign(new Error(`line ${line.n}: expected key: value`), { line: line.n });
      map[match[1]] = match[2] ? scalar(match[2]) : (lines[at] && indentOf(lines[at]) > indent ? block(indentOf(lines[at])) : null);
      Object.defineProperty(map, `__line_${match[1]}`, { value: line.n, enumerable: false });
    }
    return map;
  };
  return block(0) || {};
}

/** A layout file's value (as the Runner's ParseLayout gives it), or its problems by line. */
export function readLayout(text) {
  let doc;
  try { doc = parseLayoutYaml(text); } catch (error) { return { issues: [{ path: 'layout', line: error.line || 1, message: error.message, severity: 'error' }] }; }
  const issues = [];
  const node = (n, path) => {
    if (!n || typeof n !== 'object') { issues.push({ path, line: 1, message: 'each part is a group or a split', severity: 'error' }); return null; }
    if (n.panels) {
      for (const kind of n.panels) if (!PANELS.includes(kind)) issues.push({ path: `${path}.panels`, line: n.__line_panels || 1, message: `there is no panel "${kind}"`, severity: 'error' });
      return { type: 'group', kinds: n.panels, ...(n.minimized ? { minimized: true } : {}) };
    }
    const children = (n.children || []).map((c, i) => node(c, `${path}.children[${i}]`));
    const sizes = n.sizes || children.map(() => 1), total = sizes.reduce((a, b) => a + b, 0);
    return { type: 'split', direction: n.split, sizes: sizes.map(s => Math.round(s / total * 10000) / 10000), children };
  };
  if (!doc.name) issues.push({ path: 'name', line: 1, message: 'a layout needs a name, as the Layout menu shows it', severity: 'error' });
  const layout = node(doc.layout, 'layout');
  return issues.length ? { issues } : { value: { name: doc.name, description: doc.description || '', layout }, issues };
}

export function createConfigFake({ mutations }) {
  let revision = 1;
  const docs = new Map();
  const blank = (name, kind) => ({ name, kind, path: `${DIR}/${name}.yaml`, exists: false, error: '', stale: false, issues: [], value: undefined, revision: 1, loadedAt: now(), schemaPath: `${DIR}/${kind}.schema.json` });
  docs.set('keys', blank('keys', 'keys'));
  docs.set('settings', blank('settings', 'settings'));
  const touch = doc => { doc.revision++; doc.loadedAt = now(); revision++; };
  const wire = () => ({ directory: DIR, revision: String(revision), files: [...docs.values()].sort((a, b) => (a.kind === 'layout') - (b.kind === 'layout') || a.name.localeCompare(b.name)).map(d => ({
    name: d.name, kind: d.kind, path: d.path, exists: d.exists, error: d.error, stale: d.stale, issues: d.issues, json: d.value === undefined ? '' : JSON.stringify(d.value), revision: String(d.revision), loadedAtUnixMillis: d.loadedAt, schemaPath: d.schemaPath,
  })) });
  const keysValue = d => d.value || { preset: 'vim', bindings: [], menu: [], vim: { messageBox: false, fileEditor: false } };
  const fake = {
    docs, dir: DIR,
    /** What the Runner would load after a save in an editor: `change` replaces fields of the file (value, issues, error, stale, exists). */
    setFile(name, change) {
      const kind = name.startsWith('layouts/') ? 'layout' : name;
      const doc = docs.get(name) || blank(name, kind);
      if (kind === 'layout') doc.schemaPath = `${DIR}/layout.schema.json`;
      Object.assign(doc, { exists: true, error: '', stale: false, issues: [] }, change);
      docs.set(name, doc);
      touch(doc);
      return doc;
    },
    /** A layout file written as text, read as the Runner would (problems by line). */
    writeLayout(name, text) { const { value, issues } = readLayout(text); return fake.setFile(name, { value, issues, text }); },
    remove(name) { if (name.startsWith('layouts/')) docs.delete(name); else Object.assign(docs.get(name), { exists: false, value: undefined, issues: [], error: '' }); revision++; },
    value(name) { return docs.get(name)?.value; },
    handle(method, body) {
      if (method === 'GetConfigFiles') return { value: wire() };
      if (method !== 'UpdateConfigFile') return undefined;
      mutations.push({ method, body });
      const name = body.name, kind = name.startsWith('layouts/') ? 'layout' : name;
      if (!['keys', 'settings', 'layout'].includes(kind)) return { error: { code: 'invalid_argument', message: `no config file "${name}"` } };
      if (body.remove) { if (kind !== 'layout') return { error: { code: 'invalid_argument', message: 'only a layout file can be removed here' } }; docs.delete(name); revision++; return { value: wire() }; }
      if (body.content) {
        const { value, issues } = readLayout(body.content);
        if (!value) return { error: { code: 'failed_precondition', message: `the change has problems: ${issues.map(i => `error: line ${i.line} (${i.path}): ${i.message}`).join('; ')}` } };
        fake.setFile(name, { value, issues, text: body.content });
        return { value: wire() };
      }
      const doc = docs.get(name) || blank(name, kind);
      for (const edit of body.edits || []) {
        const value = edit.json ? JSON.parse(edit.json) : undefined, path = edit.path;
        if (kind === 'keys') {
          const keys = doc.value = keysValue(doc);
          if (path[0] === 'preset') keys.preset = value ?? 'vim';
          else if (path[0] === 'keys') {
            keys.bindings = keys.bindings.filter(b => b.action !== path[2]);
            if (value !== undefined) keys.bindings.push({ action: path[2], keys: Array.isArray(value) ? value : [value].filter(Boolean), section: path[1], line: 20 + keys.bindings.length });
          } else if (path[0] === 'leader-menu') keys.menu = (value || []).map((e, i) => ({ mode: 'send', ...e, line: 40 + i }));
          else if (path[0] === 'vim') keys.vim = { ...keys.vim, [path[1] === 'message-box' ? 'messageBox' : 'fileEditor']: !!value };
        } else if (kind === 'settings') {
          const settings = doc.value = { notifications: {}, appearance: {}, ...doc.value };
          settings.notifications = { ...settings.notifications }; settings.appearance = { ...settings.appearance };
          const [section, names] = path[0] === 'appearance' ? [settings.appearance, APPEARANCE] : [settings.notifications, NOTIFICATIONS];
          if (!names[path[1]]) return { error: { code: 'failed_precondition', message: `the change has problems: error: line 1 (${path.join('.')}): unknown setting "${path[1]}"` } };
          // The Runner's range check (internal/power/settings.go), for the one a test refuses.
          if (path[1] === 'zoom' && value !== undefined && (value < 0.5 || value > 2)) return { error: { code: 'failed_precondition', message: `the change has problems: error: line 1 (appearance.zoom): ${value} is outside 0.5 to 2` } };
          if (value === undefined) delete section[names[path[1]]]; else section[names[path[1]]] = value;
        } else if (path[0] === 'name' && doc.value) doc.value = { ...doc.value, name: value };
      }
      docs.set(name, doc);
      doc.exists = true;
      touch(doc);
      return { value: wire() };
    },
  };
  return fake;
}
