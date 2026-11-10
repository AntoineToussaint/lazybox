import { useEffect, useRef, useState } from "react";
import type { ViewStateStore } from "@lazybox/platform";
export const keyboardActions = [
  {
    id: "navigation.exit",
    label: "Leave agent for navigation",
    scope: "global",
    keys: "] ]",
  },
  {
    id: "workspace.new",
    label: "New workspace",
    scope: "global",
    keys: "] ] w",
  },
  {
    id: "conversation.new",
    label: "New conversation",
    scope: "global",
    keys: "] ] c",
  },
  { id: "snippets.open", label: "Snippets", scope: "global", keys: "] ] s" },
  {
    id: "navigation.next",
    label: "Next entry",
    scope: "navigation",
    keys: "j",
  },
  {
    id: "navigation.previous",
    label: "Previous entry",
    scope: "navigation",
    keys: "k",
  },
  {
    id: "navigation.parent",
    label: "Workspace / left pane",
    scope: "navigation",
    keys: "h",
  },
  {
    id: "navigation.open",
    label: "Open entry / agent",
    scope: "navigation",
    keys: "l",
  },
  { id: "chat.send", label: "Send message", scope: "agent", keys: "Enter" },
] as const;
export type Action = (typeof keyboardActions)[number]["id"];
export type Keymap = Record<Action, string>;
export function keyboardPreset(preset = "vim"): Keymap {
  const map = Object.fromEntries(
    keyboardActions.map((a) => [a.id, a.keys]),
  ) as Keymap;
  if (preset === "arrows") {
    map["navigation.next"] = "ArrowDown";
    map["navigation.previous"] = "ArrowUp";
    map["navigation.parent"] = "ArrowLeft";
    map["navigation.open"] = "ArrowRight";
  }
  if (preset === "tmux")
    for (const id of Object.keys(map) as Action[]) {
      map[id] = map[id].replace("] ]", "Ctrl+b");
      if (id.startsWith("navigation.") && id !== "navigation.exit")
        map[id] =
          "Ctrl+b " +
          ((
            {
              "navigation.next": "ArrowDown",
              "navigation.previous": "ArrowUp",
              "navigation.parent": "ArrowLeft",
              "navigation.open": "ArrowRight",
            } as Partial<Keymap>
          )[id] || map[id]);
    }
  return map;
}
export function validateKeymap(map: Keymap): string {
  for (const action of keyboardActions) {
    const value = map[action.id]?.trim();
    if (!value) return `Set a shortcut for ${action.label}.`;
    if(value.split(/\s+/).some(key=>!/^(?:(?:Ctrl|Meta|Alt|Shift)\+)*(?:[^\s+]|Enter|Escape|Tab|Space|ArrowDown|ArrowUp|ArrowLeft|ArrowRight|Backspace|Delete|Home|End|PageUp|PageDown|F[1-9][0-9]?)$/.test(key)))return `Invalid key in ${action.label}. Use a character or a key such as Ctrl+b or ArrowDown.`;
    if (value.split(/\s+/).length > 4)
      return "Shortcuts support up to four keys.";
  }
  for (const a of keyboardActions)
    for (const b of keyboardActions)
      if (
        a.id !== b.id &&
        map[a.id].trim().replace(/\s+/g," ") === map[b.id].trim().replace(/\s+/g," ") &&
        (a.scope === b.scope || a.scope === "global" || b.scope === "global")
      )
        return `${a.label} conflicts with ${b.label}.`;
  return "";
}
const storageKey = "lazybox:keyboard:v2";
export function readKeymap(store?: ViewStateStore): Keymap {
  try {
    const raw = JSON.parse(
      (store?.getItem(storageKey) ?? localStorage.getItem(storageKey)) ||
        "null",
    );
    const map = { ...keyboardPreset(), ...raw };
    return validateKeymap(map) ? keyboardPreset() : normalizeKeymap(map);
  } catch {
    return keyboardPreset();
  }
}
function normalizeKeymap(map:Keymap):Keymap{return Object.fromEntries(keyboardActions.map(a=>[a.id,map[a.id].trim().replace(/\s+/g," ")])) as Keymap;}
export function saveKeymap(map: Keymap, store?: ViewStateStore) {
  const error = validateKeymap(map);
  if (error) throw Error(error);
  map=normalizeKeymap(map);
  if (store) store.setItem(storageKey, JSON.stringify(map));
  else localStorage.setItem(storageKey, JSON.stringify(map));
  window.dispatchEvent(new Event("lazybox-keymap"));
}
export function invokeAction(id: Action) {
  window.dispatchEvent(new CustomEvent("lazybox-action", { detail: id }));
}
export function useKeyboardAction(id: Action, callback: () => void) {
  const ref = useRef(callback);
  ref.current = callback;
  useEffect(() => {
    const listener = (event: Event) => {
      if ((event as CustomEvent).detail === id) ref.current();
    };
    window.addEventListener("lazybox-action", listener);
    return () => window.removeEventListener("lazybox-action", listener);
  }, [id]);
}
function eventKey(e: KeyboardEvent) {
  return `${e.ctrlKey ? "Ctrl+" : ""}${e.metaKey ? "Meta+" : ""}${e.altKey ? "Alt+" : ""}${e.shiftKey && (e.key.length > 1 || /^[a-z]$/i.test(e.key)) ? "Shift+" : ""}${e.key === " " ? "Space" : e.key.length === 1 ? e.key.toLowerCase() : e.key}`;
}
export function useKeyboardNavigation(store?: ViewStateStore, enabled = true) {
  const [hint, setHint] = useState("");
  useEffect(() => {
    if (!enabled) return;
    let map = readKeymap(store),
      sequence: string[] = [],
      typed: HTMLTextAreaElement | undefined;
    const reset = () => {
      sequence = [];
      typed = undefined;
      setHint("");
    };
    const reload = () => {
      map = readKeymap(store);
      reset();
    };
    const keydown = (e: KeyboardEvent) => {
      if (
        e.isComposing ||
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
      const candidates = keyboardActions.filter(
        (a) =>
          a.scope === "global" ||
          (a.scope === "agent" && agent) ||
          (a.scope === "navigation" && navigation),
      );
      const key = eventKey(e);
      let next = [...sequence, key];
      let matches = candidates.filter(
        (a) =>
          map[a.id].split(/\s+/).slice(0, next.length).join(" ") ===
          next.join(" "),
      );
      if (!matches.length) {
        reset();
        next = [key];
        matches = candidates.filter((a) => map[a.id].split(/\s+/)[0] === key);
        if (!matches.length) return;
      }
      // A lone closing bracket remains ordinary text. Only the completed leader removes it.
      if (agent && sequence.length === 0 && key === "]") {
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
      const exact = matches.find((a) => map[a.id] === next.join(" "));
      if (exact) invokeAction(exact.id);
      const descendants = matches.filter(
        (a) => map[a.id].split(/\s+/).length > next.length,
      );
      if (descendants.length)
        setHint(
          descendants
            .map(
              (a) =>
                `${map[a.id].split(/\s+/).slice(next.length).join(" ")} ${a.label}`,
            )
            .join(" · "),
        );
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
