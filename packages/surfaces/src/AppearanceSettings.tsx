import { DISPLAY_PRESETS, useDisplayDetail } from './display-detail';
import { DisplayPresetPicker } from './DisplayPresetPicker';
import { writeActivityDefault } from './activity-detail';
import { useEffect, useMemo, useState } from 'react';
import type { ViewStateStore, ZoomHost } from '@lazybox/platform';
import { SettingsGroup, SettingsHeader, SettingsRow, settingsError } from './settings-parts';
import { Icon } from './Icons';
import { RichContent } from './RichContent';
import { useAppearance, useAppearancePreferences } from './appearance/useAppearance';
import { useUserThemes, adoptUserThemes } from './appearance/useUserThemes';
import { BUILTIN_THEMES, resolveTheme, type ThemeDefinition } from './appearance/themes';
import { TOKENS, type Scheme, type TokenGroup } from './appearance/tokens';
import { CONTRAST_CHOICES, DEFAULT_PREFERENCES, DENSITY_PRESETS, TYPOGRAPHY_LIMITS, effectiveTypography, findTheme, validFont, type AppearancePreferences, type Density, type Mode, type Motion, type Typography } from './appearance/preferences';
import { themeTemplate, userThemeId } from './appearance/user-themes';
import { normalizeColor, parseColor } from './appearance/color';
import { useRunner } from './RunnerContext';
import { runAction } from './actions';
import { ConfirmDialog } from './ConfirmDialog';
import { ConfigFileStatus } from './ConfigFileStatus';
import { writeConfigSoon } from './config-writer';
import './appearance-settings.css';

const ACCENTS: { name: string; value: string }[] = [
  { name: 'Lavender', value: '#bdb1ed' }, { name: 'Blue', value: '#7fb0f0' }, { name: 'Teal', value: '#6fcfc4' },
  { name: 'Green', value: '#8fd0a9' }, { name: 'Amber', value: '#e2b765' }, { name: 'Coral', value: '#ef9a86' }, { name: 'Rose', value: '#ea9ac0' },
];
const UI_FONTS = ['Inter', 'SF Pro Text', 'IBM Plex Sans', 'Helvetica Neue', 'Segoe UI', 'Roboto'];
const CODE_FONTS = ['JetBrains Mono', 'Fira Code', 'SF Mono', 'Menlo', 'Cascadia Code', 'IBM Plex Mono', 'Berkeley Mono', 'Iosevka'];
const SPACING: { id: Density; label: string }[] = [{ id: 'compact', label: 'Compact' }, { id: 'comfortable', label: 'Comfortable' }, { id: 'relaxed', label: 'Relaxed' }];
const WIDTHS = [0, 70, 80, 100, 120];
const SAMPLE = 'Fixed the parser: **nested lists** keep their indentation now, and `parseBlock` stops at a blank line.\n\nWhat I checked:\n- the 40 existing cases still pass\n- two new cases cover the bug you reported\n\n```ts\nconst list = parseBlock(lines, { nested: true }); // 3 items\n```';

/** One choice among a few, as buttons that read like a segmented control. */
function Segmented<T extends string | number>({ label, value, options, onChange }: { label: string; value: T; options: { id: T; label: string }[]; onChange(next: T): void }) {
  return <div className="appearance-segmented" role="radiogroup" aria-label={label}>
    {options.map(option => <button key={String(option.id)} type="button" role="radio" aria-checked={value === option.id} className={value === option.id ? 'is-selected' : ''} onClick={() => onChange(option.id)}>{option.label}</button>)}
  </div>;
}

/** A small picture of a theme: its sidebar, canvas, text and status colours. */
function ThemePreview({ theme }: { theme: ThemeDefinition }) {
  const t = useMemo(() => resolveTheme(theme).tokens, [theme]);
  return <span className="theme-preview" aria-hidden="true" style={{ background: t.bg, borderColor: t.line }}>
    <span className="theme-preview-sidebar" style={{ background: t['surface-sunken'], borderColor: t['line-subtle'] }}>
      <i style={{ background: t['surface-selected'] }}/><i style={{ background: t['text-faint'] }}/><i style={{ background: t['text-faint'] }}/>
    </span>
    <span className="theme-preview-canvas" style={{ background: t.surface }}>
      <i style={{ background: t.text, width: '70%' }}/><i style={{ background: t['text-muted'], width: '90%' }}/><i style={{ background: t['text-muted'], width: '55%' }}/>
      <span className="theme-preview-chips"><b style={{ background: t.accent }}/><b style={{ background: t.ok }}/><b style={{ background: t.warn }}/><b style={{ background: t.danger }}/></span>
    </span>
  </span>;
}

function ThemeCard({ theme, selected, active, onSelect, note }: { theme: ThemeDefinition; selected: boolean; active: boolean; onSelect(): void; note?: string }) {
  return <button type="button" role="radio" aria-checked={selected} className={`theme-card${selected ? ' is-selected' : ''}${active ? ' is-active' : ''}`} onClick={onSelect} title={note}>
    <ThemePreview theme={theme}/>
    <span className="theme-card-label"><strong>{theme.name}</strong><small>{theme.source?.kind === 'user' ? theme.source.file : 'Built in'}{note ? ` · ${note}` : active ? ' · showing' : ''}</small></span>
    {selected && <span className="theme-card-check"><Icon name="check" size={13}/></span>}
  </button>;
}

function NumberField({ id, value, onChange, limits, step = 1, unit }: { id: string; value: number; onChange(next: number): void; limits: [number, number]; step?: number; unit?: string }) {
  return <span className="appearance-number">
    <input id={id} type="range" min={limits[0]} max={limits[1]} step={step} value={value} onChange={e => onChange(Number(e.target.value))}/>
    <input type="number" aria-label={`${id.replace(/-/g, ' ')} value`} min={limits[0]} max={limits[1]} step={step} value={value} onChange={e => { const n = Number(e.target.value); if (Number.isFinite(n) && n >= limits[0] && n <= limits[1]) onChange(n); }}/>
    {unit && <span className="appearance-unit">{unit}</span>}
  </span>;
}

function Reset({ shown, onClick, what }: { shown: boolean; onClick(): void; what: string }) {
  return shown ? <button type="button" className="appearance-reset" aria-label={`Reset ${what}`} title={`Back to the theme's ${what}`} onClick={onClick}><Icon name="refresh" size={12}/></button> : null;
}

export function AppearanceSettings({ store, zoom }: { store?: ViewStateStore; zoom?: ZoomHost }) {
  const [preferences, update] = useAppearancePreferences(store);
  const { theme: shown, system } = useAppearance();
  const { api } = useRunner();
  const user = useUserThemes(true);
  const display = useDisplayDetail(store);
  const [status, setStatus] = useState('');
  const [error, setError] = useState('');
  const [deleting, setDeleting] = useState('');
  const [reference, setReference] = useState(false);
  const [filter, setFilter] = useState('');
  const [zoomLevel, setZoomLevel] = useState<number>();
  useEffect(() => { if (user.loaded) adoptUserThemes(store, user.files); }, [user.files, user.loaded, store]);
  /** The desktop zoom, set here: kept by the host, and in settings.yaml. */
  const setZoom = (level: number) => { if (!zoom) return; void zoom.set(level).then(set => { setZoomLevel(set); writeConfigSoon('zoom', { file: 'settings', zoom: set }); }, () => undefined); };
  useEffect(() => {
    if (!zoom) return;
    void zoom.get().then(setZoomLevel, () => {});
    return zoom.subscribe(setZoomLevel);
  }, [zoom]);

  const userThemes = user.files.flatMap(f => f.theme ? [f.theme] : []);
  // A theme in use whose file is broken (or gone) stays listed, as its last good version.
  const kept = Object.values(preferences.userThemes || {}).filter(t => !userThemes.some(u => u.id === t.id));
  const definitions = [...BUILTIN_THEMES, ...userThemes, ...kept];
  const scheme: Scheme = preferences.mode === 'system' ? system : preferences.mode;
  const current = findTheme(scheme === 'dark' ? preferences.darkTheme : preferences.lightTheme, preferences, scheme);
  const typography = effectiveTypography(preferences, current);
  const themeTypography = effectiveTypography({ ...preferences, typography: {} }, current);
  const set = (change: Partial<AppearancePreferences>) => update(p => ({ ...p, ...change }));
  const type = (change: Partial<Typography>) => update(p => ({ ...p, typography: { ...p.typography, ...change } }));
  const unset = (...keys: (keyof Typography)[]) => update(p => { const next = { ...p.typography }; for (const k of keys) delete next[k]; return { ...p, typography: next }; });
  const choose = (theme: ThemeDefinition) => update(p => ({
    ...p,
    ...(theme.scheme === 'dark' ? { darkTheme: theme.id } : { lightTheme: theme.id }),
    // A click shows the result: a fixed mode follows the chosen theme's appearance.
    mode: p.mode === 'system' ? 'system' : theme.scheme,
    userThemes: theme.source?.kind === 'user' ? { ...(p.userThemes || {}), [theme.id]: theme } : p.userThemes,
  }));
  const createTheme = async () => {
    setError(''); setStatus('');
    const base = `my-${scheme}-theme`;
    const taken = new Set(user.files.map(f => f.file.replace(/\.(json|toml)$/i, '')));
    let name = base; for (let i = 2; taken.has(name); i++) name = `${base}-${i}`;
    try {
      const created = await api.createTheme({ name, content: themeTemplate(`My ${scheme} theme`, scheme, shown.tokens, current.source?.kind === 'builtin' ? current.id : current.extends) }, { timeoutMs: 10000 });
      setStatus(`Created ${created.path}. Edit it in any editor: Lazybox reloads it each time you save.`);
      user.reload();
    } catch (failure) { setError(settingsError(failure)); }
  };
  const deleteTheme = async (file: string) => {
    const folder = await api.deleteTheme({ file }, { timeoutMs: 10000 });
    const id = userThemeId(file);
    // JSON and TOML can share a stem. Preserve the selection if its other file remains.
    if (!folder.files.some(theme => userThemeId(theme.name) === id)) {
      update(p => {
        const snapshot = { ...p.userThemes };
        const parent = snapshot[id]?.extends;
        delete snapshot[id];
        return {
          ...p, userThemes: snapshot,
          darkTheme: p.darkTheme === id ? parent || 'dark' : p.darkTheme,
          lightTheme: p.lightTheme === id ? parent || 'light' : p.lightTheme,
        };
      });
    }
    user.reload();
    setDeleting('');
    setStatus(`Deleted ${file}.`);
  };
  const copy = (text: string, what: string) => { void navigator.clipboard?.writeText(text).then(() => setStatus(`Copied ${what}.`), () => setStatus('')); };
  const overrides = Object.entries(preferences.tokens || {});
  const groups = [...new Set(TOKENS.map(t => t.group))] as TokenGroup[];

  return <section className="appearance-settings">
    <SettingsHeader title="Appearance" description="How Lazybox looks. Every change shows at once, in every window. Colour overrides stay on this device."
      actions={<button type="button" onClick={() => { update(p => ({ ...DEFAULT_PREFERENCES, userThemes: p.userThemes })); setStatus('Appearance reset to the defaults.'); }}><Icon name="refresh" size={14}/> Reset all</button>}/>
    <ConfigFileStatus name="settings" what="Your appearance settings" section="appearance"/>
    {error && <p className="settings-error" role="alert">{error}</p>}
    <p className="appearance-status" role="status">{status}</p>

    <SettingsGroup title="Information detail" description="Choose a starting point for the workspace sidebar and conversations. Tool activity can still be customized in Settings → Chat.">
      <DisplayPresetPicker value={display.preset} onChange={preset => {
        display.save({ preset });
        writeActivityDefault(store, DISPLAY_PRESETS.find(choice => choice.id === preset)!.activity);
      }}/>
      <label className="settings-row"><span className="settings-row-label">Show repository and Git details in the sidebar</span>
        <input type="checkbox" role="switch" checked={display.workspaceDetails} onChange={event => display.customize({ workspaceDetails: event.target.checked })}/>
      </label>
    </SettingsGroup>
    <SettingsGroup title="Theme" description="System follows your computer's light or dark appearance, using the theme you pick for each.">
      <SettingsRow label="Appearance" description={preferences.mode === 'system' ? `Following the system: ${system} now.` : undefined}>
        <Segmented<Mode> label="Appearance" value={preferences.mode} onChange={mode => set({ mode })} options={[{ id: 'system', label: 'System' }, { id: 'light', label: 'Light' }, { id: 'dark', label: 'Dark' }]}/>
      </SettingsRow>
      {(['dark', 'light'] as Scheme[]).map(s => <div className="theme-choices" key={s}>
        <h4 id={`themes-${s}`}>{s === 'dark' ? 'Dark themes' : 'Light themes'}</h4>
        <div className="theme-grid" role="radiogroup" aria-labelledby={`themes-${s}`}>
          {definitions.filter(d => d.scheme === s).map(d => <ThemeCard key={d.id} theme={d} selected={(s === 'dark' ? preferences.darkTheme : preferences.lightTheme) === d.id} active={shown.id === d.id} onSelect={() => choose(d)} note={kept.includes(d) ? 'last good version' : undefined}/>)}
        </div>
      </div>)}
      <div className="theme-folder">
        <div>
          <strong>Your themes</strong>
          <small>{user.error || (user.directory ? <>A <code>.toml</code> or <code>.json</code> file in <code className="theme-folder-path">{user.directory}</code> is a theme. Saved changes apply within seconds.</> : 'Reading the themes folder…')}</small>
        </div>
        <div className="theme-folder-actions">
          {user.directory && <button type="button" onClick={() => copy(user.directory, 'the folder path')}><Icon name="copy" size={13}/> Copy path</button>}
          <button type="button" onClick={() => void createTheme()} disabled={!user.directory}><Icon name="plus" size={13}/> New theme from current</button>
        </div>
      </div>
      {user.files.length > 0 && <ul className="theme-file-list" aria-label="Theme files">
        {user.files.map(({ file, theme }) => <li key={file}>
          <span>{theme?.name || file}</span>
          <button type="button" aria-label={`Delete theme ${file}`} onClick={() => setDeleting(file)}><Icon name="close" size={13}/><span>Delete</span></button>
        </li>)}
      </ul>}
      {deleting && <ConfirmDialog title="Delete theme?" subject={deleting} confirm="Delete theme" working="Deleting…" onClose={() => setDeleting('')} onConfirm={() => deleteTheme(deleting)}>
        This permanently removes the theme file from the Runner.
      </ConfirmDialog>}
      {user.files.filter(f => f.errors.length || f.warnings.length).map(f => <div key={f.file} className={`theme-problems${f.errors.length ? ' is-error' : ''}`} role={f.errors.length ? 'alert' : undefined}>
        <strong>{f.file}</strong>{f.errors.length ? <span> is not loaded:</span> : <span> loaded, with notes:</span>}
        <ul>{[...f.errors, ...f.warnings].map(problem => <li key={problem}>{problem}</li>)}</ul>
      </div>)}
    </SettingsGroup>

    <SettingsGroup title="Colour">
      <SettingsRow label="Accent colour" description="Buttons, selection, focus and links. The whole accent family follows it.">
        <span className="accent-picker" role="radiogroup" aria-label="Accent colour">
          <button type="button" role="radio" aria-checked={!preferences.accent} className={`accent-swatch is-theme${!preferences.accent ? ' is-selected' : ''}`} title="The theme's accent" aria-label="Theme accent" onClick={() => set({ accent: undefined })} style={{ background: resolveTheme(current).tokens.accent }}/>
          {ACCENTS.map(a => <button key={a.value} type="button" role="radio" aria-checked={preferences.accent === a.value} aria-label={a.name} title={a.name} className={`accent-swatch${preferences.accent === a.value ? ' is-selected' : ''}`} style={{ background: a.value }} onClick={() => set({ accent: a.value })}/>)}
          <label className="accent-custom" title="Any colour"><span className="sr-only">Custom accent colour</span><input type="color" value={normalizeColor(preferences.accent || shown.tokens.accent).slice(0, 7)} onChange={e => set({ accent: e.target.value })}/></label>
        </span>
      </SettingsRow>
      <SettingsRow label="Minimum contrast" htmlFor="appearance-contrast" description="Text too faint against its background is made just dark or light enough to reach it. Surfaces never change.">
        <select id="appearance-contrast" value={preferences.minContrast} onChange={e => set({ minContrast: Number(e.target.value) })}>
          {CONTRAST_CHOICES.map(c => <option key={c} value={c}>{c === 0 ? 'Off: exactly as the theme says' : c === 3 ? '3:1 · large text' : c === 4.5 ? '4.5:1 · WCAG AA (recommended)' : '7:1 · WCAG AAA'}</option>)}
        </select>
      </SettingsRow>
      <SettingsRow label="Colour overrides" description={<>Change any token for this device, on top of every theme. {overrides.length ? '' : 'None yet.'}</>} stacked>
        <div className="token-overrides">
          {overrides.map(([name, value]) => <div key={name} className="token-override">
            <code>--{name}</code>
            <input type="color" aria-label={`${name} colour`} value={normalizeColor(value).slice(0, 7)} onChange={e => set({ tokens: { ...preferences.tokens, [name]: e.target.value } })}/>
            <input aria-label={`${name} value`} defaultValue={value} key={value} onBlur={e => { if (parseColor(e.target.value)) set({ tokens: { ...preferences.tokens, [name]: e.target.value.trim() } }); }}/>
            <button type="button" className="appearance-reset" aria-label={`Remove the ${name} override`} onClick={() => { const next = { ...preferences.tokens }; delete next[name]; set({ tokens: Object.keys(next).length ? next : undefined }); }}><Icon name="close" size={12}/></button>
          </div>)}
          <select aria-label="Add a colour override" value="" onChange={e => { const name = e.target.value; if (name) set({ tokens: { ...preferences.tokens, [name]: shown.tokens[name] } }); }}>
            <option value="">Add an override…</option>
            {groups.map(g => <optgroup key={g} label={g}>{TOKENS.filter(t => t.group === g && t.kind === 'color' && !preferences.tokens?.[t.name]).map(t => <option key={t.name} value={t.name}>{t.name}</option>)}</optgroup>)}
          </select>
        </div>
      </SettingsRow>
      <div className="token-reference">
        <button type="button" className="token-reference-toggle" aria-expanded={reference} onClick={() => setReference(r => !r)}><Icon name="chevronRight" size={13}/> Token reference <small>{TOKENS.length} tokens: use these names in a theme file</small></button>
        {reference && <>
          <input className="token-filter" type="search" aria-label="Filter tokens" placeholder="Filter tokens" value={filter} onChange={e => setFilter(e.target.value)}/>
          {groups.map(g => {
            const rows = TOKENS.filter(t => t.group === g && (!filter || t.name.includes(filter.toLowerCase()) || t.description.toLowerCase().includes(filter.toLowerCase())));
            return rows.length ? <section key={g} className="token-group"><h4>{g}</h4><table><tbody>{rows.map(t => <tr key={t.name}>
              <td>{t.kind === 'color' ? <span className="token-swatch" style={{ background: shown.tokens[t.name] }}/> : null}</td>
              <td><button type="button" className="token-name" title="Copy the CSS variable" onClick={() => copy(`var(--${t.name})`, `var(--${t.name})`)}>{t.name}</button>{t.base && <span className="token-base" title="Set this and the derived tokens follow">base</span>}</td>
              <td>{t.description}</td>
              <td><code>{shown.tokens[t.name]}</code></td>
            </tr>)}</tbody></table></section> : null;
          })}
        </>}
      </div>
    </SettingsGroup>

    <SettingsGroup title="Text">
      <SettingsRow label="Interface font" htmlFor="appearance-ui-font" description="Any font installed on this computer. Empty uses the system font.">
        <span className="appearance-inline"><input id="appearance-ui-font" list="appearance-ui-fonts" placeholder="System font" defaultValue={typography.uiFont} key={`ui-${typography.uiFont}`} onBlur={e => { const v = e.target.value.trim(); if (validFont(v)) type({ uiFont: v }); }} onKeyDown={e => { if (e.key === 'Enter') e.currentTarget.blur(); }}/><datalist id="appearance-ui-fonts">{UI_FONTS.map(f => <option key={f} value={f}/>)}</datalist><Reset shown={preferences.typography.uiFont !== undefined} what="interface font" onClick={() => unset('uiFont')}/></span>
      </SettingsRow>
      <SettingsRow label="Interface text size" htmlFor="appearance-ui-size">
        <span className="appearance-inline"><NumberField id="appearance-ui-size" value={typography.uiFontSize} limits={TYPOGRAPHY_LIMITS.uiFontSize} onChange={v => type({ uiFontSize: v })} unit="px"/><Reset shown={typography.uiFontSize !== themeTypography.uiFontSize} what="interface text size" onClick={() => unset('uiFontSize')}/></span>
      </SettingsRow>
      <SettingsRow label="Code font" htmlFor="appearance-code-font" description="The editor, code blocks, diffs and command output. Empty uses the system monospace, and JetBrains Mono (bundled) for command output.">
        <span className="appearance-inline"><input id="appearance-code-font" list="appearance-code-fonts" placeholder="System monospace" defaultValue={typography.codeFont} key={`code-${typography.codeFont}`} onBlur={e => { const v = e.target.value.trim(); if (validFont(v)) type({ codeFont: v }); }} onKeyDown={e => { if (e.key === 'Enter') e.currentTarget.blur(); }}/><datalist id="appearance-code-fonts">{CODE_FONTS.map(f => <option key={f} value={f}/>)}</datalist><Reset shown={preferences.typography.codeFont !== undefined} what="code font" onClick={() => unset('codeFont')}/></span>
      </SettingsRow>
      <SettingsRow label="Code text size" htmlFor="appearance-code-size">
        <span className="appearance-inline"><NumberField id="appearance-code-size" value={typography.codeFontSize} limits={TYPOGRAPHY_LIMITS.codeFontSize} onChange={v => type({ codeFontSize: v })} unit="px"/><Reset shown={typography.codeFontSize !== themeTypography.codeFontSize} what="code text size" onClick={() => unset('codeFontSize')}/></span>
      </SettingsRow>
      <SettingsRow label="Ligatures" description="Join character pairs such as => and != when the code font has them.">
        <span className="settings-row-control settings-switch"><input type="checkbox" role="switch" aria-label="Ligatures" checked={typography.ligatures} onChange={e => type({ ligatures: e.target.checked })}/></span>
      </SettingsRow>
      <SettingsRow label="Zoom" htmlFor="appearance-zoom" description={zoom ? <>Saved app-wide zoom. <kbd>⌘=</kbd> <kbd>⌘−</kbd> <kbd>⌘0</kbd> adjust the focused window until it closes.</> : <>Use your browser's zoom: <kbd>⌘+</kbd> <kbd>⌘−</kbd> <kbd>⌘0</kbd>. The desktop app keeps its zoom in settings.yaml.</>}>
        {zoom ? <span className="appearance-inline appearance-zoom">
          <button type="button" aria-label="Zoom out" onClick={() => setZoom(Math.round(((zoomLevel ?? 1) - 0.1) * 100) / 100)}><Icon name="zoomOut" size={14}/></button>
          <input id="appearance-zoom" type="range" min={50} max={200} step={5} value={Math.round((zoomLevel ?? 1) * 100)} onChange={e => setZoom(Number(e.target.value) / 100)}/>
          <output aria-live="polite">{Math.round((zoomLevel ?? 1) * 100)}%</output>
          <button type="button" aria-label="Zoom in" onClick={() => setZoom(Math.round(((zoomLevel ?? 1) + 0.1) * 100) / 100)}><Icon name="zoomIn" size={14}/></button>
          <Reset shown={(zoomLevel ?? 1) !== 1} what="zoom" onClick={() => setZoom(1)}/>
        </span> : <span className="appearance-hint">Browser zoom</span>}
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="Layout and motion">
      <SettingsRow label="Density" description={<>Row heights in lists and the sidebar. Also sets the chat’s line and paragraph spacing (<button type="button" className="link" onClick={() => runAction('settings.open', 'chat')}>Settings → Chat</button>).</>}>
        <Segmented<Density> label="Density" value={typography.density} onChange={density => update(p => { const next = { ...p.typography, density }; delete next.chatLineHeight; delete next.chatParagraphGap; return { ...p, typography: next }; })} options={SPACING}/>
      </SettingsRow>
      <SettingsRow label="Motion" description="Reduce turns off animations and smooth scrolling. System follows your computer's setting.">
        <Segmented<Motion> label="Motion" value={preferences.motion} onChange={motion => set({ motion })} options={[{ id: 'system', label: 'System' }, { id: 'reduce', label: 'Reduce' }, { id: 'full', label: 'Full' }]}/>
      </SettingsRow>
    </SettingsGroup>
  </section>;
}

/**
 * Settings → Chat → Chat text: size, line and paragraph spacing and line
 * width of conversations, with a sample message. Part of the appearance
 * preferences (a theme or a density may suggest them; the person's win).
 */
export function ChatTextSettings({ store }: { store?: ViewStateStore }) {
  const [preferences, update] = useAppearancePreferences(store);
  const { system } = useAppearance();
  const scheme: Scheme = preferences.mode === 'system' ? system : preferences.mode;
  const current = findTheme(scheme === 'dark' ? preferences.darkTheme : preferences.lightTheme, preferences, scheme);
  const typography = effectiveTypography(preferences, current);
  const themeTypography = effectiveTypography({ ...preferences, typography: {} }, current);
  const type = (change: Partial<Typography>) => update(p => ({ ...p, typography: { ...p.typography, ...change } }));
  const unset = (...keys: (keyof Typography)[]) => update(p => { const next = { ...p.typography }; for (const k of keys) delete next[k]; return { ...p, typography: next }; });
  const lineHeightChoice = SPACING.find(s => DENSITY_PRESETS[s.id].chatLineHeight === typography.chatLineHeight)?.id;
  return <div className="appearance-settings chat-text-settings">
    <SettingsGroup title="Chat text" description="How conversations read, kept in settings.yaml with your appearance. The sample is drawn exactly as a message is.">
      <SettingsRow label="Size" htmlFor="appearance-chat-size">
        <span className="appearance-inline"><NumberField id="appearance-chat-size" value={typography.chatFontSize} limits={TYPOGRAPHY_LIMITS.chatFontSize} onChange={v => type({ chatFontSize: v })} unit="px"/><Reset shown={typography.chatFontSize !== themeTypography.chatFontSize} what="chat text size" onClick={() => unset('chatFontSize')}/></span>
      </SettingsRow>
      <SettingsRow label="Line spacing" description="Space between the lines of a paragraph.">
        <span className="appearance-inline appearance-wrap">
          <Segmented<string> label="Line spacing" value={lineHeightChoice || 'custom'} onChange={id => { if (id !== 'custom') type({ chatLineHeight: DENSITY_PRESETS[id as Density].chatLineHeight }); }} options={[...SPACING.map(s => ({ id: s.id as string, label: s.label })), { id: 'custom', label: 'Custom' }]}/>
          <input className="appearance-small-number" type="number" aria-label="Line spacing value" min={TYPOGRAPHY_LIMITS.chatLineHeight[0]} max={TYPOGRAPHY_LIMITS.chatLineHeight[1]} step={0.05} value={typography.chatLineHeight} onChange={e => { const n = Number(e.target.value); if (n >= 1.1 && n <= 2.2) type({ chatLineHeight: n }); }}/>
          <Reset shown={typography.chatLineHeight !== themeTypography.chatLineHeight} what="line spacing" onClick={() => unset('chatLineHeight')}/>
        </span>
      </SettingsRow>
      <SettingsRow label="Paragraph spacing" htmlFor="appearance-chat-gap">
        <span className="appearance-inline"><NumberField id="appearance-chat-gap" value={typography.chatParagraphGap} limits={TYPOGRAPHY_LIMITS.chatParagraphGap} onChange={v => type({ chatParagraphGap: v })} unit="px"/><Reset shown={typography.chatParagraphGap !== themeTypography.chatParagraphGap} what="paragraph spacing" onClick={() => unset('chatParagraphGap')}/></span>
      </SettingsRow>
      <SettingsRow label="Line width" htmlFor="appearance-chat-width" description="Long lines are hard to read on a wide window: cap them, centred.">
        <span className="appearance-inline">
          <select id="appearance-chat-width" value={WIDTHS.includes(typography.chatMaxWidth) ? typography.chatMaxWidth : 'custom'} onChange={e => { if (e.target.value !== 'custom') type({ chatMaxWidth: Number(e.target.value) }); }}>
            {WIDTHS.map(w => <option key={w} value={w}>{w ? `${w} characters` : 'Full width'}</option>)}
            {!WIDTHS.includes(typography.chatMaxWidth) && <option value="custom">{typography.chatMaxWidth} characters</option>}
          </select>
          <Reset shown={typography.chatMaxWidth !== themeTypography.chatMaxWidth} what="line width" onClick={() => unset('chatMaxWidth')}/>
        </span>
      </SettingsRow>
      <div className="chat-sample" aria-label="Chat text preview">
        <div className="messages-content"><article className="message message_delta"><RichContent text={SAMPLE}/></article></div>
      </div>
    </SettingsGroup>

  </div>;
}
