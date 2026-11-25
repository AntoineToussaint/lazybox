import { createContext, useState } from 'react';
import { humanError } from './human-error';
import { useRunner } from './RunnerContext';

export const CanSaveHelpSnippet = createContext(false);

export function SaveHelpSnippet({ yaml }: { yaml: string }) {
  const { api } = useRunner();
  const [saving, setSaving] = useState(false);
  const [key, setKey] = useState('');
  const [error, setError] = useState('');
  async function save() {
    setSaving(true);
    setError('');
    try {
      const entry = await api.saveSnippet({ yaml });
      setKey(entry.key);
      window.dispatchEvent(new Event('lazybox-snippets-changed'));
    } catch (reason) {
      setError(humanError(reason));
    } finally {
      setSaving(false);
    }
  }
  return <>{error && <span role="alert">{error}</span>}
    <button type="button" disabled={saving || !!key} onClick={() => void save()}>
      {key ? `Saved /${key}` : saving ? 'Saving…' : 'Save snippet'}
    </button></>;
}
