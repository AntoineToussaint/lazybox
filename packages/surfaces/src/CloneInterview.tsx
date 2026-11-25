import { useEffect, useRef, useState } from 'react';
import type { CloneProfile } from '@lazybox/protocol';
import { ConfirmDialog } from './ConfirmDialog';
import { useRunner } from './RunnerContext';

const fields = [
  ['style', 'How do you like code structured?'],
  ['testing', 'What verification do you expect?'],
  ['askWhen', 'Which decisions should agents bring to you?'],
] as const;

export function CloneInterview({ onClose }: { onClose(): void }) {
  const { api } = useRunner();
  const [profile, setProfile] = useState<CloneProfile>();
  const [learning, setLearning] = useState(false);
  const [error, setError] = useState('');
  const [responses, setResponses] = useState<Record<string, string>>({});
  const [forgotten, setForgotten] = useState<string[]>([]);
  const analysis = useRef<AbortController | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    api.getCloneProfile({}, { signal: controller.signal }).then(setProfile, err => {
      if (!controller.signal.aborted) setError(String(err));
    });
    return () => { controller.abort(); analysis.current?.abort(); };
  }, [api]);

  const learn = async () => {
    const controller = new AbortController();
    analysis.current = controller;
    setLearning(true);
    setError('');
    try {
      const learned = await api.getCloneProfile({ learnFromHistory: true }, { signal: controller.signal, timeoutMs: 120000 });
      setProfile(current => current && { ...current, suggestions: learned.suggestions, questions: learned.questions });
    } catch (err) {
      if (!controller.signal.aborted) setError(String(err));
    } finally {
      setLearning(false);
    }
  };
  const save = async () => {
    if (!profile) return;
    const interview = profile.questions.flatMap(question => {
      const answer = responses[question]?.trim();
      return answer ? [question + ' ' + answer] : [];
    });
    const style = [profile.style, ...interview].filter(Boolean).join('\n');
    await api.saveCloneProfile({ profile: { ...profile, style }, forgetAnswerIds: forgotten });
    onClose();
  };
  return <ConfirmDialog title="Teach Clone Me" tone="primary" confirm="Save preferences" working="Saving…"
    canConfirm={!!profile && !learning} error={error} onClose={onClose} onConfirm={save}
    details={profile ? <div className="clone-interview">
      <button type="button" disabled={learning} onClick={() => void learn()}>
        {learning ? 'Reviewing conversations…' : 'Learn from recent conversations'}
      </button>
      {profile.questions.map(question => <label key={question}>{question}
        <textarea value={responses[question] || ''} onChange={event => setResponses({ ...responses, [question]: event.target.value })}/>
      </label>)}
      {profile.suggestions.length > 0 && <fieldset><legend>Suggested preferences</legend>
        {profile.suggestions.map(line => <button type="button" key={line} disabled={profile.style.split('\n').includes(line)}
          onClick={() => setProfile({ ...profile, style: [profile.style, line].filter(Boolean).join('\n') })}>{line}</button>)}
      </fieldset>}
      {fields.map(([key, label]) => <label key={key}>{label}
        <textarea value={profile[key]} onChange={event => setProfile({ ...profile, [key]: event.target.value })}/>
      </label>)}
      {profile.rememberedAnswers.length > 0 && <fieldset><legend>Remembered answers</legend>
        {profile.rememberedAnswers.map(item => <label key={item.id}>
          <input type="checkbox" checked={forgotten.includes(item.id)} onChange={ev => setForgotten(ev.target.checked ? [...forgotten, item.id] : forgotten.filter(id => id !== item.id))}/>
          Forget in {item.workspace}: {item.question} — {item.answer}
        </label>)}
      </fieldset>}
    </div> : 'Loading your profile…'}>
    Save the preferences you want your coding agents to use.
  </ConfirmDialog>;
}
