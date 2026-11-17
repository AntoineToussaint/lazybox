import { LeaderMenuSettings } from "./LeaderMenuSettings";
import { useEffect, useRef, useState } from "react";
import { ConfigFileStatus } from "./ConfigFileStatus";
import type { ViewStateStore } from "@lazybox/platform";
import {
  keyboardActions,
  keyboardPreset,
  readKeymap,
  saveKeymap,
  saveVimPreferences,
  useVimPreferences,
  type Action,
  type VimPreferences,
} from "./keyboard";
import { Icon } from "./Icons";
import { actionDefinition, actionGroups, isMac } from "./actions";
import { SettingsGroup, SettingsHeader, SettingsRow } from "./settings-parts";

type ShortcutDraft = { map: ReturnType<typeof readKeymap>; preset?: string };
/**
 * Unsaved shortcut edits, per window and Runner: Settings unmounts a section
 * when another is chosen, and coming back finds the edits where they were.
 */
const drafts = new Map<string, ShortcutDraft>();
const draftKey = (store?: ViewStateStore) => store?.root || store?.key || "";

export function KeyboardSettings({ store }: { store?: ViewStateStore }) {
  const [saved] = useState(() => drafts.get(draftKey(store)));
  const [map, setMapState] = useState(() => saved?.map || readKeymap(store)),
    [status, setStatus] = useState(""),
    [tone, setTone] = useState<"saved" | "error" | "">(""),
    [dirty, setDirty] = useState(!!saved),
    // The preset picked here, which keys.yaml records with the save.
    [preset, setPresetState] = useState<string | undefined>(saved?.preset);
  const keep = (next: ShortcutDraft) => drafts.set(draftKey(store), next);
  const setMap = (next: ShortcutDraft["map"]) => { setMapState(next); keep({ map: next, preset }); };
  // keys.yaml saved in an editor shows here at once, unless you are mid-edit.
  const dirtyRef = useRef(dirty); dirtyRef.current = dirty;
  useEffect(() => {
    const reload = () => { if (!dirtyRef.current) setMapState(readKeymap(store)); };
    window.addEventListener("lazybox-keymap", reload);
    return () => window.removeEventListener("lazybox-keymap", reload);
  }, [store]);
  return (
    <section className="keyboard-settings">
      <SettingsHeader
        title="Keyboard"
        description={<>Sequences and single keys work outside text fields; chords such as <code>{isMac ? "Meta+k" : "Ctrl+k"}</code> work everywhere. Separate a sequence with spaces (<code>] ] s</code>), and alternatives with <code>or</code> (<code>{isMac ? "Meta+k" : "Ctrl+k"} or ] ] p</code>). Leave a field empty for no shortcut. Press <kbd>?</kbd> anywhere to see them all.</>}
      />
      <ConfigFileStatus name="keys" what="Your shortcuts, ]] menu and Vim choice" />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          try {
            void saveKeymap(map, store, { preset, quiet: true }).then((result) => {
              setStatus(result.saved || !result.message ? `Shortcuts saved.${result.message ? ` ${result.message.replace(/^Saved to (.*)\.$/, "$1 updated.")}` : ""}` : result.message);
              setTone(result.saved || !result.message ? "saved" : "error");
            });
            setDirty(false);
            setPresetState(undefined);
            drafts.delete(draftKey(store));
          } catch (error) {
            setStatus(String(error).replace(/^Error:\s*/, ""));
            setTone("error");
          }
        }}
      >
        <SettingsGroup title="Navigation shortcuts">
          <SettingsRow label="Start from a preset" htmlFor="keyboard-preset" description="Fills in every shortcut below. Nothing changes until you save.">
            <select
              id="keyboard-preset"
              value={preset ?? "custom"}
              onChange={(e) => {
                const next = keyboardPreset(e.target.value);
                setMapState(next);
                setPresetState(e.target.value);
                keep({ map: next, preset: e.target.value });
                setDirty(true);
                setStatus("");
              }}
            >
              <option value="custom" disabled>Your shortcuts</option>
              <option value="vim">Vim · ]] and h j k l</option>
              <option value="tmux">Ctrl+b prefix · arrow keys</option>
              <option value="arrows">Arrow keys · ]] leader</option>
            </select>
          </SettingsRow>
        </SettingsGroup>
        {actionGroups.map((group) => (
          <SettingsGroup key={group} title={group}>
            {keyboardActions.filter((a) => a.group === group).map((a) => (
              <SettingsRow key={a.id} label={a.label} htmlFor={`shortcut-${a.id}`} description={actionDefinition(a.id)?.fixed ? "Cannot be changed, so it always works: it puts back the default layout and these defaults, with Undo." : undefined}>
                <input
                  id={`shortcut-${a.id}`}
                  className="shortcut-input"
                  readOnly={!!actionDefinition(a.id)?.fixed}
                  aria-label={a.label}
                  placeholder="No shortcut"
                  spellCheck={false}
                  autoComplete="off"
                  value={map[a.id]}
                  onChange={(e) => {
                    setMap({ ...map, [a.id as Action]: e.target.value });
                    setDirty(true);
                    setStatus("");
                  }}
                />
              </SettingsRow>
            ))}
          </SettingsGroup>
        ))}
        <div className="settings-savebar">
          <p role="status" className={tone === "saved" && status ? "settings-saved" : status ? "settings-inline-error" : "settings-dirty"}>
            {tone === "saved" && status && <Icon name="check" size={14} />}
            {status || (dirty ? "Unsaved changes" : "")}
          </p>
          <button className="primary">Save shortcuts</button>
        </div>
      </form>
      <VimSettings store={store} />
      <LeaderMenuSettings store={store} />
    </section>
  );
}

/** Vim editing is opt-in and applies as soon as it is switched. */
function VimSettings({ store }: { store?: ViewStateStore }) {
  const preferences = useVimPreferences(store);
  const [status, setStatus] = useState("");
  const toggle = (key: keyof VimPreferences, on: boolean, label: string) => {
    try {
      saveVimPreferences({ ...preferences, [key]: on }, store);
      setStatus(`Vim editing ${on ? "on" : "off"} in the ${label}.`);
    } catch (error) {
      setStatus(String(error));
    }
  };
  const row = (key: keyof VimPreferences, label: string, where: string, detail: string) => (
    <label className="settings-row vim-toggle">
      <span className="settings-row-text"><span className="settings-row-label">{label}</span><small>{detail}</small></span>
      <span className="settings-row-control settings-switch">
        <input type="checkbox" role="switch" checked={preferences[key]} onChange={(e) => toggle(key, e.target.checked, where)} />
      </span>
    </label>
  );
  return (
    <section className="vim-settings" aria-labelledby="vim-settings-title">
      <SettingsGroup
        title="Vim editing"
        description={<>Modal editing where you type, applied at once. Esc switches to normal mode; the {readKeymap(store)["navigation.exit"]} leader keeps working in every mode.</>}
      >
        <span id="vim-settings-title" className="sr-only">Vim editing</span>
        {row("composer", "Vim editing in the message box", "message box", "Starts in insert mode. In normal mode Enter sends; i a o, h j k l w b e, d c y with counts, p, u and Ctrl-r edit the draft.")}
        {row("editor", "Vim editing in the file editor", "file editor", "Full Vim keys in the file window, with a status line. :w saves, :q closes (asking first about unsaved changes), :wq saves and closes.")}
      </SettingsGroup>
      <p role="status" className="settings-saved vim-status-line">{status}</p>
    </section>
  );
}
