import { useId } from 'react';
import { DISPLAY_PRESETS, type DisplayPreset } from './display-detail';
import './display-detail.css';

export function DisplayPresetPicker({ value, onChange }: { value: DisplayPreset; onChange(value: DisplayPreset): void }) {
  const group = useId();
  const selected = DISPLAY_PRESETS.find(preset => preset.id === value) || DISPLAY_PRESETS[1];
  return <>
    <fieldset className="display-presets">
      <legend>Information detail</legend>
      {DISPLAY_PRESETS.map(preset => <label key={preset.id}>
        <input type="radio" name={group} checked={value === preset.id} onChange={() => onChange(preset.id)}/>
        <span>{preset.label}</span>
      </label>)}
    </fieldset>
    <div className="display-preview" aria-label={`${selected.label} preview`}>
      <p>{selected.description}</p>
      <div><strong>API cleanup</strong>{selected.workspaceDetails && <span> · feature/api-cleanup</span>}</div>
      {selected.id === 'power' ? <div>Read handler.ts · Run tests · 2 min ago</div> : selected.id === 'standard' ? <div>Checked 2 steps · Tests passed</div> : <div>Tests passed</div>}
    </div>
  </>;
}
