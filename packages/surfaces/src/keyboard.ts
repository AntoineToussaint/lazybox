import { readPreference, writePreference } from "./view-state";
import { useEffect, useRef, useState } from "react";
import type { ViewStateStore } from "@lazybox/platform";
import { actionDefinition, actionDefinitions, bindings, canonicalBinding, canonicalKey, defaultKeys, isChord, isLoneModifier, isMac, joinBindings, resolveMod, runAction, useAction, type ActionId } from "./actions";
import { writeConfig, type ConfigWriteResult } from "./config-writer";
/** Every keyboard action, from the action registry (actions.ts). */
export const keyboardActions = actionDefinitions.map((definition) => ({ id: definition.id as Action, label: definition.title as string, scope: definition.scope as string, keys: definition.keys as string, group: definition.group as string }));
export type Action = ActionId;
export type Keymap = Record<Action, string>;
const tmuxNavigation: Partial<Record<Action, string>> = { "navigation.next": "ArrowDown", "navigation.previous": "ArrowUp", "navigation.parent": "ArrowLeft", "navigation.open": "ArrowRight" };
/** A full keymap from a preset: Vim (`]]` and h j k l), arrows, or a tmux-style Ctrl+b prefix. */
export function keyboardPreset(preset = "vim", mac = isMac): Keymap {
  const map = Object.fromEntries(actionDefinitions.map((a) => [a.id, defaultKeys(a, mac)])) as Keymap;
  if (preset === "arrows") Object.assign(map, tmuxNavigation);
  if (preset === "tmux")
    for (const id of Object.keys(map) as Action[]) {
      map[id] = withLeader(map[id], "Ctrl+b");
      if (tmuxNavigation[id]) map[id] = "Ctrl+b " + tmuxNavigation[id];
    }
  return map;
}
/** Rewrites the `] ]` leader of every sequence in a keymap value to another leader. */
function withLeader(value: string, leader: string): string {
  return joinBindings(bindings(value).map((binding) => (binding === "] ]" ? leader : binding.startsWith("] ] ") ? `${leader} ${binding.slice(4)}` : binding)));
}
const keyPattern = /^(?:(?:(?:Ctrl|Meta|Alt|Shift)\+)*(?:[^\s+]|\+|Enter|Escape|Tab|Space|ArrowDown|ArrowUp|ArrowLeft|ArrowRight|Backspace|Delete|Home|End|PageUp|PageDown|F[1-9][0-9]?)|AltRight|MetaRight)$/;
/** Two actions can fire from the same place: one of them global, or both in one scope. */
const overlaps = (a: { scope: string }, b: { scope: string }) => a.scope === b.scope || a.scope === "global" || b.scope === "global";
export function validateKeymap(map: Keymap): string {
  for (const action of keyboardActions) {
    const value = map[action.id]?.trim() || "";
    if (!value) {
      if (actionDefinition(action.id)?.required) return `Set a shortcut for ${action.label}.`;
      continue;
    }
    for (const binding of bindings(value)) {
      if (binding.split(/\s+/).some((key) => !keyPattern.test(key))) return `Invalid key in ${action.label}. Use a character or a key such as Ctrl+b or ArrowDown.`;
      if (binding.split(/\s+/).length > 4) return "Shortcuts support up to four keys.";
      // A key held down to talk cannot be a sequence, nor a key you type.
      const hold = actionDefinition(action.id)?.hold;
      if (hold && (binding.includes(" ") || !(/(?:^|\+)(?:Ctrl|Meta|Alt)\+|^F\d+$/.test(binding) || isLoneModifier(binding)))) return `${action.label} needs one key held with ⌥, ⌃ or ⌘, a function key, or Right ⌥ or Right ⌘ alone, such as Alt+Space.`;
      // A modifier alone is a key held to talk, nothing else.
      if (!hold && binding.split(/\s+/).some(isLoneModifier)) return `${action.label} cannot be a modifier alone: only a key held to talk can be Right ⌥ or Right ⌘.`;
    }
  }
  const owners: { binding: string; action: (typeof keyboardActions)[number] }[] = [];
  for (const a of keyboardActions)
    for (const binding of new Set(bindings(map[a.id]).map(canonicalBinding))) {
      const taken = owners.find((owner) => owner.binding === binding && owner.action.id !== a.id && overlaps(a, owner.action));
      if (taken) return `${a.label} conflicts with ${taken.action.label}.`;
      owners.push({ binding, action: a });
    }
  return "";
}
/**
 * The actions that cannot be rebound (the reset chord) take their default
 * keys, and no other action keeps those keys: whatever a keymap holds, the
 * way back still works.
 */
export function withFixedKeys(map: Keymap, mac = isMac): Keymap {
  const fixed = keyboardActions.filter((a) => actionDefinition(a.id)?.fixed);
  if (!fixed.length) return map;
  const next = { ...map };
  const taken = new Set<string>();
  for (const a of fixed) { next[a.id] = defaultKeys(actionDefinition(a.id)!, mac); for (const binding of bindings(next[a.id])) taken.add(canonicalBinding(binding)); }
  for (const a of keyboardActions) if (!actionDefinition(a.id)?.fixed) next[a.id] = joinBindings(bindings(next[a.id]).filter((binding) => !taken.has(canonicalBinding(binding))));
  return next;
}
const storageKey = "keyboard:v2";
/**
 * Keymaps saved before the action registry (no version) hold only the first
 * thirteen actions; version 2 predates `] ] v` (activity detail) and `] ] b` (web panel);
 * version 3 predates `] ] r` (run the last command block); version 4 predates the
 * voice keys (⌥Space, ⌥⇧Space); version 5 predates the panel-group keys (⌘⌥ arrows,
 * ⌘⌥⇧ arrows, ⌃⌘ arrows, ⌃1–9, ⌘T) and the reset chord; version 6 predates ⌘N
 * (a new task) and ⌘⇧N (a new conversation in this workspace); version 7 predates ⌘⇧L
 * (next layout); version 8 predates Right ⌥ alone for dictation and ⌘↵ to send;
 * version 9 predates Send now, which took ⌘↵ from Send message (idle, it still sends).
 */
const keymapVersion = 10;
/**
 * A saved keymap, brought up to date. Saved keys always win. An action the
 * save predates takes its default keys, in the saved leader (`Ctrl+b p` for a
 * tmux keymap), minus any key that would collide with a saved one or with an
 * entry of the person's own `]]` menu. Go to file gains Mod+P the same way,
 * and New conversation Mod+Shift+N.
 */
export function migrateKeymap(raw: unknown, menu: readonly MenuEntry[] = [], mac = isMac): Keymap {
  const saved = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
  const version = typeof saved.__version === "number" ? saved.__version : 1;
  const defaults = keyboardPreset("vim", mac);
  const map = { ...defaults };
  const kept = new Set<Action>();
  for (const a of keyboardActions)
    if (typeof saved[a.id] === "string") {
      map[a.id] = joinBindings(bindings(saved[a.id] as string).map(canonicalBinding));
      kept.add(a.id);
    }
  if (!kept.size || version >= keymapVersion) return withFixedKeys(map, mac);
  // ⌘↵ is Send now's since version 10: a saved Send message gives it up, unless it is that action's only key.
  const modEnter = resolveMod("Mod+Enter", mac), sendKeys = bindings(map["chat.send"]);
  if (kept.has("chat.send") && sendKeys.includes(modEnter) && sendKeys.length > 1) map["chat.send"] = joinBindings(sendKeys.filter((binding) => binding !== modEnter));
  const leader = map["navigation.exit"];
  const menuKeys = new Set(menu.map((entry) => canonicalBinding(`${leader} ${entry.key}`)));
  const added = keyboardActions.map((a) => ({ action: a, candidates: kept.has(a.id) ? (a.id === "files.find" && version < 2 ? [resolveMod("Mod+p", mac)] : a.id === "conversation.new" && version < 7 ? [resolveMod("Mod+Shift+n", mac)] : a.id === "voice.dictate" && version < 9 ? ["AltRight"] : []) : bindings(withLeader(defaults[a.id], leader)) }));
  for (const { action } of added) if (!kept.has(action.id)) map[action.id] = "";
  for (const { action, candidates } of added)
    for (const binding of candidates) {
      const clash = menuKeys.has(binding) || keyboardActions.some((other) => other.id !== action.id && overlaps(action, other) && bindings(map[other.id]).some((value) => canonicalBinding(value) === binding));
      // Right ⌥ comes first: it is the key the app names (⌥Space is often another app's).
      const first = isLoneModifier(binding) && kept.has(action.id);
      if (!clash && !bindings(map[action.id]).includes(binding)) map[action.id] = joinBindings(first ? [binding, ...bindings(map[action.id])] : [...bindings(map[action.id]), binding]);
    }
  return withFixedKeys(map, mac);
}
export function readKeymap(store?: ViewStateStore): Keymap {
  try {
    const map = migrateKeymap(JSON.parse(readPreference(store, storageKey) || "null"), readMenu(store));
    return validateKeymap(map) ? keyboardPreset() : normalizeKeymap(map);
  } catch {
    return keyboardPreset();
  }
}
function normalizeKeymap(map: Keymap): Keymap {
  return withFixedKeys(Object.fromEntries(keyboardActions.map((a) => [a.id, joinBindings(bindings(map[a.id]).map(canonicalBinding))])) as Keymap);
}
/**
 * Saves the keymap and writes it to keys.yaml (POWER.md, "Config files");
 * the promise says whether the file now holds it. `preset` is the preset the
 * person started from, which the file records.
 */
export function saveKeymap(map: Keymap, store?: ViewStateStore, options: { preset?: string; quiet?: boolean } = {}): Promise<ConfigWriteResult> {
  const error = validateKeymap(withFixedKeys(map));
  if (error) throw Error(error);
  map = normalizeKeymap(map);
  writePreference(store, storageKey, JSON.stringify({ ...map, __version: keymapVersion }));
  window.dispatchEvent(new Event("lazybox-keymap"));
  return writeConfig({ file: "keys", preset: options.preset }, { quiet: options.quiet });
}
/**
 * Every shortcut back to its default (the reset chord), in keys.yaml too.
 * Returns the undo, which puts back exactly what was saved.
 */
export function resetKeymap(store?: ViewStateStore): () => void {
  const saved = readPreference(store, storageKey);
  writePreference(store, storageKey, JSON.stringify({ ...keyboardPreset(), __version: keymapVersion }));
  window.dispatchEvent(new Event("lazybox-keymap"));
  void writeConfig({ file: "keys", preset: "vim" });
  return () => {
    writePreference(store, storageKey, saved ?? JSON.stringify({ ...keyboardPreset(), __version: keymapVersion }));
    window.dispatchEvent(new Event("lazybox-keymap"));
    void writeConfig({ file: "keys" });
  };
}
/**
 * What keys.yaml declares, applied without writing it back (the file is
 * where it came from). True when anything changed.
 */
export function applyKeysFromFile(store: ViewStateStore | undefined, next: { map: Keymap; menu: MenuEntry[]; vim: VimPreferences }): boolean {
  const map = normalizeKeymap(next.map);
  const sameMap = JSON.stringify(normalizeKeymap(readKeymap(store))) === JSON.stringify(map);
  const sameMenu = JSON.stringify(readMenu(store)) === JSON.stringify(next.menu);
  const vim = readVimPreferences(store), sameVim = vim.composer === next.vim.composer && vim.editor === next.vim.editor;
  if (!sameMap) writePreference(store, storageKey, JSON.stringify({ ...map, __version: keymapVersion }));
  if (!sameMenu) writePreference(store, menuKey, JSON.stringify(next.menu));
  if (!sameVim) writePreference(store, vimKey, JSON.stringify({ composer: !!next.vim.composer, editor: !!next.vim.editor }));
  if (sameMap && sameMenu && sameVim) return false;
  window.dispatchEvent(new Event("lazybox-keymap"));
  return true;
}
/**
 * Whether the element with focus owns this chord, so the keymap leaves it
 * alone: the file editor's ⌘⌥← / ⌘⌥→ (with ⇧ to move) step through its own
 * tabs, and in the code view ⌘⌥↑ / ⌘⌥↓ (Ctrl+Alt or Alt+Shift elsewhere) add a
 * cursor or copy the line, as in every code editor.
 */
export function ownedByFocus(binding: string, target: Pick<Element, "closest"> | null): boolean {
  if (!target) return false;
  const parts = canonicalKey(binding).split("+"), name = parts.at(-1) || "";
  const mod = parts.includes("Meta") || parts.includes("Ctrl"), alt = parts.includes("Alt");
  if (alt && mod && (name === "ArrowLeft" || name === "ArrowRight") && target.closest(".editor-area")) return true;
  if (alt && (name === "ArrowUp" || name === "ArrowDown") && target.closest(".monaco-editor")) return true;
  return false;
}
/** The keys of an action, one string per alternative. */
export function actionKeys(map: Keymap, id: Action): string[] {
  return bindings(map[id]);
}
/** Single chords a keydown listener runs at once (sequence prefixes such as tmux's Ctrl+b excluded). */
export function chordBindings(map: Keymap): Map<string, Action> {
  const prefixes = new Set(keyboardActions.flatMap((a) => bindings(map[a.id]).filter((b) => b.includes(" ")).map((b) => b.split(" ")[0])));
  const chords = new Map<string, Action>();
  for (const a of keyboardActions) for (const binding of bindings(map[a.id])) if (isChord(binding) && !prefixes.has(binding) && a.scope === "global" && !actionDefinition(a.id)?.hold) chords.set(binding, a.id);
  return chords;
}
/** The first key of every global sequence (the leader, such as "]"), as
 * KeyboardEvent.key: Vim editing leaves these to the keyboard dispatcher. */
export function leaderKeys(map: Keymap): string[] {
  return [...new Set(keyboardActions.filter((a) => a.scope === "global").flatMap((a) => bindings(map[a.id]).filter((binding) => binding.includes(" ")).map((binding) => binding.split(" ")[0])).map((key) => (key === "Space" ? " " : key)))];
}
/** Opt-in Vim editing, separately for the message box and the file editor. */
export type VimPreferences = { composer: boolean; editor: boolean };
const vimKey = "vim-editing:v1";
export function readVimPreferences(store?: ViewStateStore): VimPreferences {
  try {
    const raw = JSON.parse(readPreference(store, vimKey) || "{}");
    return { composer: raw?.composer === true, editor: raw?.editor === true };
  } catch {
    return { composer: false, editor: false };
  }
}
export function saveVimPreferences(preferences: VimPreferences, store?: ViewStateStore): Promise<ConfigWriteResult> {
  writePreference(store, vimKey, JSON.stringify({ composer: !!preferences.composer, editor: !!preferences.editor }));
  window.dispatchEvent(new Event("lazybox-keymap"));
  return writeConfig({ file: "keys" }, { quiet: true });
}
export function useVimPreferences(store?: ViewStateStore): VimPreferences {
  const [preferences, setPreferences] = useState(() => readVimPreferences(store));
  useEffect(() => {
    const reload = () => setPreferences((previous) => {
      const next = readVimPreferences(store);
      return previous.composer === next.composer && previous.editor === next.editor ? previous : next;
    });
    reload();
    window.addEventListener("lazybox-keymap", reload);
    window.addEventListener("storage", reload);
    return () => {
      window.removeEventListener("lazybox-keymap", reload);
      window.removeEventListener("storage", reload);
    };
  }, [store]);
  return preferences;
}
/** A user-defined entry in the ]] menu: one key that sends or inserts a snippet. */
export type MenuEntry = { key: string; label: string; snippet: string; mode: "send" | "insert" };
/** What the ]] menu shows while a sequence is in progress. */
export type KeyHint = { prefix: string; items: { keys: string; label: string }[] };
const menuKey = "leader-menu:v1";
const builtInMenuKeys = (map: Keymap) => {
  const leader = bindings(map["navigation.exit"])[0] || "] ]";
  return keyboardActions.flatMap((a) => bindings(map[a.id])).filter((keys) => keys.startsWith(leader + " ")).map((keys) => keys.slice(leader.length + 1));
};
export function readMenu(store?: ViewStateStore): MenuEntry[] {
  try {
    const raw = JSON.parse(readPreference(store, menuKey) || "[]");
    return Array.isArray(raw) ? raw.filter((e) => e && typeof e.key === "string" && typeof e.snippet === "string" && typeof e.label === "string").map((e) => ({ key: e.key, label: e.label, snippet: e.snippet, mode: e.mode === "insert" ? "insert" : "send" })) : [];
  } catch {
    return [];
  }
}
export function validateMenu(entries: MenuEntry[], map: Keymap): string {
  const taken = new Set(builtInMenuKeys(map));
  for (const e of entries) {
    if (!/^(?:[a-z0-9]|Shift\+[a-z])$/.test(e.key)) return `Menu key "${e.key}" must be one letter or digit.`;
    if (taken.has(e.key)) return `Menu key "${e.key}" is already used.`;
    if (!e.snippet.trim()) return `Choose a snippet for "${e.key}".`;
    if (e.label.length > 60) return `Label for "${e.key}" is too long.`;
    taken.add(e.key);
  }
  return entries.length > 36 ? "The menu holds up to 36 entries." : "";
}
export function saveMenu(entries: MenuEntry[], store?: ViewStateStore): Promise<ConfigWriteResult> {
  const error = validateMenu(entries, readKeymap(store));
  if (error) throw Error(error);
  writePreference(store, menuKey, JSON.stringify(entries));
  window.dispatchEvent(new Event("lazybox-keymap"));
  return writeConfig({ file: "keys" }, { quiet: true });
}
/**
 * Extra entries under the leader that run code of their own: the user's
 * actions (POWER.md) and "Run an action…". A provider is read on every key, so
 * a reloaded actions file shows at once. Keys the built-in actions or the
 * person's own ]] entries already use win; leaderConflicts names the losers.
 */
export type LeaderExtension = { id: string; key: string; label: string; run(): void };
let leaderExtensions: () => LeaderExtension[] = () => [];
export function setLeaderExtensions(provider: () => LeaderExtension[]): () => void {
  leaderExtensions = provider;
  return () => { if (leaderExtensions === provider) leaderExtensions = () => []; };
}
/** The keys under the leader already taken, by what. */
export function takenLeaderKeys(map: Keymap, menu: MenuEntry[]): Map<string, string> {
  const taken = new Map<string, string>();
  const leader = bindings(map["navigation.exit"])[0] || "] ]";
  // Every alternative binding of an action ("a or b") takes its key.
  for (const a of keyboardActions) for (const keys of bindings(map[a.id])) if (keys.startsWith(leader + " ")) taken.set(keys.slice(leader.length + 1), a.label);
  for (const e of menu) taken.set(e.key, e.label || e.snippet);
  return taken;
}
/** Extensions whose key is taken, with what took it. */
export function leaderConflicts(extensions: LeaderExtension[], map: Keymap, menu: MenuEntry[]): { extension: LeaderExtension; takenBy: string }[] {
  const taken = takenLeaderKeys(map, menu);
  return extensions.flatMap(extension => taken.has(extension.key) ? [{ extension, takenBy: taken.get(extension.key)! }] : []);
}
function usableExtensions(map: Keymap, menu: MenuEntry[]): LeaderExtension[] {
  const taken = takenLeaderKeys(map, menu), seen = new Set<string>();
  return leaderExtensions().filter(e => { if (taken.has(e.key) || seen.has(e.key)) return false; seen.add(e.key); return true; });
}

/** Asks the focused chat to send or insert a snippet chosen from the ]] menu. */
export function useMenuSnippet(callback: (entry: MenuEntry) => void) {
  const ref = useRef(callback);
  ref.current = callback;
  useEffect(() => {
    const listener = (event: Event) => ref.current((event as CustomEvent<MenuEntry>).detail);
    window.addEventListener("lazybox-menu", listener);
    return () => window.removeEventListener("lazybox-menu", listener);
  }, []);
}

/** Runs an action of the registry (actions.ts). */
export function invokeAction(id: Action) {
  runAction(id);
}
/** Registers what an action does while the component is mounted; see useAction in actions.ts. */
export function useKeyboardAction(id: Action, callback: () => void, enabled?: () => boolean) {
  useAction(id, callback, enabled);
}
function eventKey(e: KeyboardEvent) {
  return `${e.ctrlKey ? "Ctrl+" : ""}${e.metaKey ? "Meta+" : ""}${e.altKey ? "Alt+" : ""}${e.shiftKey && (e.key.length > 1 || /^[a-z]$/i.test(e.key)) ? "Shift+" : ""}${e.key === " " ? "Space" : e.key.length === 1 ? e.key.toLowerCase() : e.key}`;
}
/**
 * A key that belongs to an input method (Japanese, Chinese, Korean…): while it
 * composes, and the Enter that commits its text. WebKit sends that Enter after
 * compositionend, with `isComposing` false and only keyCode 229 to tell.
 */
export const imeKey = (e: Pick<KeyboardEvent, 'isComposing' | 'keyCode'>) => e.isComposing || e.keyCode === 229;
export function useKeyboardNavigation(store?: ViewStateStore, enabled = true) {
  const [hint, setHint] = useState<KeyHint | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let map = readKeymap(store),
      menu = readMenu(store),
      sequence: string[] = [],
      typed: HTMLTextAreaElement | undefined;
    const reset = () => {
      sequence = [];
      typed = undefined;
      setHint(null);
    };
    const reload = () => {
      map = readKeymap(store);
      menu = readMenu(store);
      reset();
    };
    const keydown = (e: KeyboardEvent) => {
      // A key another handler consumed (a Vim command, say) still ends a pending sequence.
      if (e.defaultPrevented && sequence.length) reset();
      if (
        imeKey(e) ||
        e.repeat ||
        e.defaultPrevented ||
        document.querySelector("dialog[open]")
      )
        return;
      const target = e.target as HTMLElement,
        agent = target.matches("[data-agent-composer]"),
        navigation = !!target.closest(".collection-panel");
      if (e.key === "Escape") {
        reset();
        return;
      }
      if (
        !agent &&
        (target.matches("input,textarea,select") || target.isContentEditable)
      ) {
        reset();
        return;
      }
      const leader = bindings(map["navigation.exit"])[0] || "] ]";
      // Typing in the message box: a bare character (such as "?") is text, not a shortcut.
      const typing = agent && !["normal", "visual"].includes(target.dataset.vimMode || "");
      const chords = chordBindings(map);
      // User entries live under the leader, next to the built-in actions.
      // Single chords (Mod+K) belong to the chord listener (useActionKeys).
      const candidates: { id: string; label: string; keys: string; entry?: MenuEntry; run?: () => void }[] = [
        ...keyboardActions
          // Push-to-talk keys belong to the voice layer (its keyup ends the dictation).
          .filter((a) => !actionDefinition(a.id)?.hold)
          .filter((a) => a.scope === "global" || (a.scope === "agent" && agent) || (a.scope === "navigation" && navigation))
          .flatMap((a) => bindings(map[a.id]).filter((keys) => !chords.has(keys) && !(typing && keys.length === 1 && keys !== "]")).map((keys) => ({ id: a.id as string, label: a.label as string, keys }))),
        ...menu.map((entry) => ({ id: "menu:" + entry.key, label: entry.label || entry.snippet, keys: `${leader} ${entry.key}`, entry })),
        ...usableExtensions(map, menu).map((extension) => ({ id: "extension:" + extension.id, label: extension.label, keys: `${leader} ${extension.key}`, run: extension.run })),
      ];
      const key = eventKey(e);
      let next = [...sequence, key];
      let matches = candidates.filter(
        (a) =>
          a.keys.split(/\s+/).slice(0, next.length).join(" ") ===
          next.join(" "),
      );
      if (!matches.length) {
        reset();
        next = [key];
        matches = candidates.filter((a) => a.keys.split(/\s+/)[0] === key);
        if (!matches.length) return;
      }
      // A lone closing bracket remains ordinary text. Only the completed leader removes it.
      // In Vim normal or visual mode the message box takes no typing, so the bracket is only the leader.
      const literal = agent && !["normal", "visual"].includes(target.dataset.vimMode || "");
      if (literal && sequence.length === 0 && key === "]") {
        sequence = next;
        typed = target as HTMLTextAreaElement;
        return;
      }
      e.preventDefault();
      if (typed && key === "]" && sequence.length === 1) {
        const position = typed.selectionStart;
        if (typed.value[position - 1] === "]") {
          const value =
            typed.value.slice(0, position - 1) + typed.value.slice(position);
          Object.getOwnPropertyDescriptor(
            HTMLTextAreaElement.prototype,
            "value",
          )!.set!.call(typed, value);
          typed.setSelectionRange(position - 1, position - 1);
          typed.dispatchEvent(new Event("input", { bubbles: true }));
        }
        typed = undefined;
      }
      sequence = next;
      const exact = matches.find((a) => a.keys === next.join(" "));
      if (exact?.entry) window.dispatchEvent(new CustomEvent("lazybox-menu", { detail: exact.entry }));
      else if (exact?.run) exact.run();
      else if (exact) runAction(exact.id, undefined, "keys");
      const descendants = matches.filter((a) => a.keys.split(/\s+/).length > next.length);
      if (descendants.length)
        setHint({
          prefix: next.join(" "),
          items: descendants.map((a) => ({ keys: a.keys.split(/\s+/).slice(next.length).join(" "), label: a.label })),
        });
      else reset();
    };
    const navigate = (event: Event) => {
      const id = (event as CustomEvent<Action>).detail;
      if (!id.startsWith("navigation.")) return;
      const entries = [
        ...document.querySelectorAll<HTMLElement>(
          ".collection-panel [data-navigation-item],.collection-panel .conversation-link",
        ),
      ];
      const active = document.activeElement as HTMLElement,
        index = entries.indexOf(active);
      if (id === "navigation.exit") {
        (
          entries.find((e) => e.getAttribute("aria-current") === "page") ||
          entries[0] ||
          document.querySelector<HTMLElement>(".chat-heading button")
        )?.focus();
      }
      if (id === "navigation.next" || id === "navigation.previous") {
        entries[
          Math.max(
            0,
            Math.min(
              entries.length - 1,
              (index < 0 ? 0 : index) + (id === "navigation.next" ? 1 : -1),
            ),
          )
        ]?.focus();
      }
      if (id === "navigation.parent")
        active
          .closest(".collection-group")
          ?.querySelector<HTMLElement>(".workspace-link")
          ?.focus();
      if (id === "navigation.open") {
        active.closest(".collection-panel") && active.click();
        requestAnimationFrame(() =>
          document.querySelector<HTMLElement>("[data-agent-composer]")?.focus(),
        );
      }
    };
    document.addEventListener("keydown", keydown);
    document.addEventListener("pointerdown",reset);window.addEventListener("blur",reset);
    window.addEventListener("lazybox-action", navigate);
    window.addEventListener("lazybox-keymap", reload);
    window.addEventListener("storage", reload);
    return () => {
      document.removeEventListener("keydown", keydown);
      document.removeEventListener("pointerdown",reset);window.removeEventListener("blur",reset);
      window.removeEventListener("lazybox-action", navigate);
      window.removeEventListener("lazybox-keymap", reload);
      window.removeEventListener("storage", reload);
    };
  }, [store, enabled]);
  return hint;
}

export function useKeymap(store?:ViewStateStore){
 const [map,setMap]=useState(()=>readKeymap(store));
 useEffect(()=>{const reload=()=>setMap(readKeymap(store));window.addEventListener('lazybox-keymap',reload);window.addEventListener('storage',reload);return()=>{window.removeEventListener('lazybox-keymap',reload);window.removeEventListener('storage',reload);};},[store]);return map;
}
