import type { ViewStateStore } from '@lazybox/platform';
import { SettingsGroup, SettingsHeader, SettingsRow } from './settings-parts';
import { searchEngine, searchEngines } from './web-address';
import { saveLinkPreferences, useLinkPreferences, type LinkTarget } from './web-links';
import { isMac } from './workbench';

const alt = isMac ? '⌥' : 'Alt';
const choices: { value: LinkTarget; label: string; detail: string }[] = [
  { value: 'browser', label: 'Your browser', detail: `${alt}-click opens a link in a Lazybox Web panel instead.` },
  { value: 'viewer', label: 'A Lazybox Web panel', detail: `Beside your work, in this window. ${alt}-click opens a link in your browser instead.` },
];

/** Settings → Web & links: where a clicked link opens, and what the address bar searches with. */
export function WebSettings({ store }: { store?: ViewStateStore }) {
  const preferences = useLinkPreferences(store);
  return <section aria-labelledby="web-settings-title">
    <SettingsHeader id="web-settings-title" title="Web & links" description="Links in conversations, artifacts, pull requests and checks. A right-click on any link offers every choice."/>
    <SettingsGroup title="Clicking a web link opens it in">
      <div role="radiogroup" aria-label="Clicking a web link opens it in">
        {choices.map(choice => <label key={choice.value} className="settings-row">
          <span className="settings-row-text"><span className="settings-row-label">{choice.label}</span><small>{choice.detail}</small></span>
          <span className="settings-row-control"><input type="radio" name="web-link-target" value={choice.value} checked={preferences.target === choice.value} onChange={() => saveLinkPreferences(store, { ...preferences, target: choice.value })}/></span>
        </label>)}
      </div>
    </SettingsGroup>
    <SettingsGroup title="Web panel" description="Each Web panel keeps its own pages and history. In the desktop app it is a full browser view with its own website data, separate from the app and from your browser: sign-ins you make there stay there, it never fills in saved passwords, and downloads go to your browser.">
      <SettingsRow label="Search with" htmlFor="web-search-engine" description="What the address bar uses when you type words instead of an address.">
        <select id="web-search-engine" value={preferences.engine.id} onChange={event => saveLinkPreferences(store, { ...preferences, engine: searchEngine(event.target.value) })}>
          {searchEngines.map(engine => <option key={engine.id} value={engine.id}>{engine.name}</option>)}
        </select>
      </SettingsRow>
    </SettingsGroup>
  </section>;
}
