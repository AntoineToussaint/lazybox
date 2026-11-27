import { useEffect, useRef, useState } from 'react';
import type { CloneProfile } from '@lazybox/protocol';
import { ConfirmDialog } from './ConfirmDialog';
import { useRunner } from './RunnerContext';
import './clone-interview.css';

const fields = [
  ['style', 'How do you like code structured?'],
  ['testing', 'What verification do you expect?'],
  ['askWhen', 'Which decisions should agents bring to you?'],
] as const;
type Field = typeof fields[number][0];
const sectionNames = { style: 'Code style', testing: 'Verification', askWhen: 'Decisions to ask about' };

export function CloneInterview({ onClose }: { onClose(): void }) {
  const { api } = useRunner();
  const [profile, setProfile] = useState<CloneProfile>();
  const [suggestionField, setSuggestionField] = useState<Field>('style');
  const [learning, setLearning] = useState(false);
  const [error, setError] = useState('');
  const [responses, setResponses] = useState<Record<string, string>>({});
  const [destinations, setDestinations] = useState<Record<string, Field>>({});
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
    const next = { ...profile };
    for (const [question, response] of Object.entries(responses)) {
      const answer = response.trim();
      if (!answer) continue;
      const field = destinations[question] || 'style';
      next[field] = [next[field], question + ' ' + answer].filter(Boolean).join('\n');
    }
    await api.saveCloneProfile({ profile: next, forgetAnswerIds: forgotten });
    onClose();
  };
  const answered = Object.keys(responses).filter(question => responses[question].trim());
  const questions = [...new Set([...(profile?.questions || []), ...answered])];
  return <ConfirmDialog className="clone-profile-dialog" title="Teach Clone Me" tone="primary" confirm="Save preferences" working="Saving…"
    canConfirm={!!profile && !learning} error={error} onClose={onClose} onConfirm={save}
    details={profile ? <div className="clone-interview">
      {fields.map(([key, label]) => <label key={key}>{label}
        <textarea value={profile[key]} onChange={event => setProfile({ ...profile, [key]: event.target.value })}/>
      </label>)}
      {profile.suggestions.length > 0 && <fieldset className="clone-suggestions"><legend>Suggested code preferences</legend>
        <label>Apply suggestion to <select value={suggestionField} onChange={event => setSuggestionField(event.target.value as Field)}>
          {fields.map(([key]) => <option key={key} value={key}>{sectionNames[key]}</option>)}
        </select></label>
        {profile.suggestions.map(line => <button type="button" key={line} disabled={profile[suggestionField].split('\n').includes(line)}
          onClick={() => setProfile({ ...profile, [suggestionField]: [profile[suggestionField], line].filter(Boolean).join('\n') })}>{line}</button>)}
      </fieldset>}
      <section className="clone-history" aria-label="Conversation history">
        <button type="button" disabled={learning} onClick={() => void learn()}>
          {learning ? 'Reviewing conversations…' : 'Learn from recent conversations'}
        </button>
        {questions.map(question => <div className="clone-question" key={question}>
          <label>{question}
            <textarea value={responses[question] || ''} onChange={event => setResponses({ ...responses, [question]: event.target.value })}/>
          </label>
          <label className="clone-destination">Save answer under
            <select aria-label={'Save answer under for ' + question} value={destinations[question] || 'style'}
              onChange={event => setDestinations({ ...destinations, [question]: event.target.value as Field })}>
              {fields.map(([key]) => <option key={key} value={key}>{sectionNames[key]}</option>)}
            </select>
          </label>
        </div>)}
      </section>
      {profile.rememberedAnswers.length > 0 && <fieldset><legend>Remembered answers</legend>
        {profile.rememberedAnswers.map(item => <label key={item.id}>
          <input type="checkbox" checked={forgotten.includes(item.id)} onChange={ev => setForgotten(ev.target.checked ? [...forgotten, item.id] : forgotten.filter(id => id !== item.id))}/>
          Forget in {item.workspace}: {item.question} — {item.answer}
        </label>)}
      </fieldset>}
    </div> : 'Loading your profile…'}>
    Set how your agents write code, verify changes, and decide when to ask you.
  </ConfirmDialog>;
}
