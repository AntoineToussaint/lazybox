import { sameModel } from './slash-commands';
import { useProviderAvailability } from './useProviderAvailability';

export function ReasoningEffort({ provider, model, value, disabled, onChange }: {
  provider: string;
  model: string;
  value: string;
  disabled: boolean;
  onChange(value: string): void;
}) {
  const supported = provider === 'claude' || provider === 'codex';
  const { report } = useProviderAvailability(supported);
  if (!supported) return null;
  const models = report?.providers.find(entry => entry.provider === provider)?.models || [];
  const selected = models.find(entry => sameModel(entry, model));
  const levels = selected?.effortLevels?.length
    ? selected.effortLevels
    : [...new Set(models.flatMap(entry => entry.effortLevels || []))];
  // Claude exposes model capabilities after the worker starts. A launch
  // override is still available before then, and the CLI resolves it.
  const choices = levels.length ? levels : ['low', 'medium', 'high', 'xhigh', 'max'];
  return <label className="reasoning-effort">Reasoning effort
    <select value={value} disabled={disabled} onChange={event => onChange(event.target.value)}>
      <option value="">Provider default</option>
      {[...new Set([...choices, ...(value ? [value] : [])])].map(level =>
        <option key={level} value={level}>{level}</option>
      )}
    </select>
  </label>;
}
