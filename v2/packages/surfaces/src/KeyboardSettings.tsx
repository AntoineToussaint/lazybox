import { useState } from "react";
import type { ViewStateStore } from "@lazybox/platform";
import {
  keyboardActions,
  keyboardPreset,
  readKeymap,
  saveKeymap,
  type Action,
} from "./keyboard";
export function KeyboardSettings({ store }: { store?: ViewStateStore }) {
  const [map, setMap] = useState(() => readKeymap(store)),
    [status, setStatus] = useState("");
  return (
    <section className="keyboard-settings">
      <h2>Keyboard navigation</h2>
      <p>
        Sequences are separated by spaces. Ctrl+b is a chord; ] ] s is a
        hierarchy. Navigation keys only apply outside the composer.
      </p>
      <label>
        Preset
        <select
          defaultValue="custom"
          onChange={(e) => {
            setMap(keyboardPreset(e.target.value));
            setStatus("Save to apply this preset.");
          }}
        >
          <option value="custom" disabled>
            Custom / current
          </option>
          <option value="vim">Vim · ]] and h j k l</option>
          <option value="tmux">tmux · Ctrl+b and arrows</option>
          <option value="arrows">Arrow keys · ]] leader</option>
        </select>
      </label>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          try {
            saveKeymap(map, store);
            setStatus("Shortcuts saved.");
          } catch (error) {
            setStatus(String(error));
          }
        }}
      >
        {keyboardActions.map((a) => (
          <label key={a.id}>
            {a.label}
            <input
              aria-label={a.label}
              value={map[a.id]}
              onChange={(e) =>
                setMap({ ...map, [a.id as Action]: e.target.value })
              }
            />
          </label>
        ))}
        <button className="primary">Save shortcuts</button>
      </form>
      <p role="status">{status}</p>
    </section>
  );
}
