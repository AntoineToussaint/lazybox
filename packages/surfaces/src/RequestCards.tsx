import { useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { providerData, type RunnerClient } from '@lazybox/client-core';
import { Decision, type AgentEvent } from '@lazybox/protocol';
import { CodeBlock } from './RichContent';
import { Icon } from './Icons';
import { isSettingsUpdate, SettingsProposal } from './SettingsProposal';
import { approvalOf, decisionForKey, decisionRows, questionsOf, scopeWords, type Approval, type AnswerNote, type DecisionRow, type Question } from './request-model';
import { shortPath } from './tool-lines';
import { arrowIntoCard, CardHead, editable, Kbd, useChatKeys, useInitialFocus, type RequestPosition } from './request-parts';
import { RepositoryRequestCard } from './RepositoryRequestCard';
import { BrowserRequestCard } from './BrowserRequestCard';
import { ConversationRequestCard } from './ConversationRequestCard';
import './requests.css';
import { humanError } from './human-error';

/**
 * The agent's questions and approvals, answered in place, at least as well as
 * in the agent's own terminal UI: options to click or pick with 1–9 and the
 * arrows, "Other…" in your words, several questions at once; approvals that
 * show exactly what will run, where and how risky it looks, with Allow once,
 * the provider's own "always allow" scopes, and Deny with a reason the agent
 * is told. Keys work while the chat has focus, never while you type.
 */
export type RequestCardProps = {
  request: AgentEvent; api: RunnerClient; session: string; onError(message: string): void;
  /** The conversation's folder: paths in it are shown relative, outside it flagged. */
  root?: string;
  /** "1 of 3" when several requests wait. */
  position?: RequestPosition;
  agentName?: string;
  enableFullAccess?(): Promise<boolean>;
  /** Asks before allowing a flagged or default-to-no request (the phone does; the computer shows the flags). */
  confirmAllow?(approval: Approval): Promise<boolean>;
  /** On a paired phone: a request to start a conversation is Start or Deny, as asked. */
  remote?: boolean;
};

export function AgentRequestCard(props: RequestCardProps) {
  if (props.request.type === 'repository_request') return <RepositoryRequestCard {...props}/>;
  if (props.request.type === 'browser_request') return <BrowserRequestCard {...props}/>;
  if (props.request.type === 'conversation_request') return <ConversationRequestCard {...props}/>;
  return props.request.type === 'question' ? <QuestionCard {...props}/> : <ApprovalCard {...props}/>;
}

/** A label with `code` spans, as the providers write commands in them. */
export function CodeText({ text }: { text: string }) {
  const parts = text.split(/(`[^`]+`)/);
  return <>{parts.map((part, i) => part.length > 2 && part.startsWith('`') && part.endsWith('`') ? <code key={i}>{part.slice(1, -1)}</code> : part)}</>;
}
const SCOPE_CHIP: Record<string, string> = { conversation: 'This conversation', prefix: 'This conversation', workspace: 'This workspace', host: 'Codex network policy' };

/* ───────────────────────── Approvals ───────────────────────── */

function ApprovalCard({ request, api, session, onError, root = '', position, agentName = 'the agent', confirmAllow, enableFullAccess }: RequestCardProps) {
  const data = useMemo(() => providerData(request.providerDataJson), [request.providerDataJson]);
  const approval = useMemo(() => approvalOf(data, request.text, root), [data, request.text, root]);
  const settings = isSettingsUpdate(data.tool);
  const [waiting, setWaiting] = useState(false);
  const [denying, setDenying] = useState(false);
  const [reason, setReason] = useState('');
  const card = useRef<HTMLElement>(null);
  // Default-to-no: only Deny has a key; allowing takes a click or Tab and Enter.
  const rows = decisionRows(approval, agentName);
  const sent = useRef(false);
  const decide = async (decision: Decision, extra: { always?: string; reason?: string; fullAccess?: boolean } = {}) => {
    if (sent.current) return;
    sent.current = true; setWaiting(true);
    let permissionChanged = false;
    try {
      if (extra.fullAccess && !(await enableFullAccess?.())) { setWaiting(false); sent.current = false; return; }
      permissionChanged = !!extra.fullAccess;
      const receipt = await api.apply({ sessionId: session, operationId: crypto.randomUUID(), operation: { case: 'approval', value: { requestId: request.requestId, decision, always: extra.always || '', reason: extra.reason || '' } } });
      if (receipt.delivery === 'failed') throw new Error(receipt.error || 'Could not send the approval.');
    }
    catch (e) { onError(permissionChanged ? `Full access is enabled. The command approval could not be sent. ${humanError(e)}` : humanError(e)); setWaiting(false); sent.current = false; }
  };
  const activate = (row: DecisionRow) => {
    if (waiting) return;
    if (row.tone === 'deny') { setDenying(true); requestAnimationFrame(() => card.current?.querySelector<HTMLTextAreaElement>('.deny-reason textarea')?.focus()); return; }
    if (approval.acceptUnsupported) return;
    if (confirmAllow && (approval.risks.length || approval.defaultToNo)) { void confirmAllow(approval).then(ok => { if (ok) void decide(Decision.ACCEPT, { always: row.choice?.id }); }); return; }
    void decide(Decision.ACCEPT, { always: row.choice?.id });
  };
  const deny = () => void decide(Decision.DECLINE, { reason: reason.trim() });
  // Leaving the reason (Esc or Back) returns focus to Deny…, not to the page: the form and its Back button are gone.
  const back = () => { setDenying(false); setReason(''); requestAnimationFrame(() => card.current?.querySelector<HTMLElement>('[data-row=deny]')?.focus()); };
  const onKey = (event: { key: string }) => {
    if (denying) return false;
    const row = decisionForKey(rows, event.key);
    if (!row) return false;
    activate(row); return true;
  };
  useChatKeys(card, onKey);
  useInitialFocus(card);
  const cardKeys = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (editable(event.target) || event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      const buttons = [...card.current!.querySelectorAll<HTMLButtonElement>('.decision-row:not(:disabled)')];
      const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
      buttons[(at + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length]?.focus();
      event.preventDefault(); return;
    }
    if (onKey(event)) event.preventDefault();
  };
  // Keys answer the first waiting request; later ones show none.
  const keys = !position || position.index === 0;
  const heading = settings ? 'Change Lazybox settings' : approval.description || approval.title || `Allow ${approval.tool}?`;
  return <article ref={card} tabIndex={-1} className="request approval-card" data-risk={approval.risks.length ? 'flagged' : undefined} aria-label={`Approval required: ${heading}`} onKeyDown={cardKeys}>
    <CardHead eyebrow="Approval required" tool={approval.tool} position={position}/>
    <h3>{heading}</h3>
    {approval.title && approval.title !== heading && <p className="approval-description">{approval.title}</p>}
    {settings ? <SettingsProposal input={approval.input} api={api}/>
      : approval.command ? <div className="approval-what"><CodeBlock text={approval.command} language="bash" label="Command"/>{approval.cwd && <small className="approval-cwd mono" title={approval.cwd}>in {shortPath(approval.cwd)}</small>}</div>
      : approval.patch ? <div className="approval-what"><CodeBlock text={approval.patch} language="diff" label={`Changes${approval.file ? ' to ' + shortPath(approval.file, root) : ''} · +${approval.added} −${approval.removed}`}/></div>
      : <CodeBlock text={JSON.stringify(approval.input, null, 2)} language="json" label="Requested input"/>}
    {approval.risks.length > 0 && <ul className="approval-risks" aria-label="Worth a second look">{approval.risks.map(r => <li key={r.kind} data-kind={r.kind}><Icon name="alert" size={12}/>{r.text}</li>)}</ul>}
    <details className="request-metadata"><summary>Request details</summary><CodeBlock text={JSON.stringify(data, null, 2)} language="json"/><small className="mono">{request.requestId}</small></details>
    {!denying ? <div className="decision-rows" role="group" aria-label="Your decision">
      {rows.map(row => <button key={row.id} type="button" className="decision-row" data-row={row.tone === 'always' ? 'always' : row.id} data-tone={row.tone} aria-label={row.tone === 'deny' ? 'Deny…' : row.label} aria-keyshortcuts={[row.digit, row.key].filter(Boolean).join(' ') || undefined}
        disabled={waiting || (row.tone !== 'deny' && approval.acceptUnsupported)} title={row.tone !== 'deny' && approval.acceptUnsupported ? 'Lazybox cannot grant this kind of permission yet' : row.detail} onClick={() => activate(row)}>
        {keys && row.digit && <Kbd>{row.digit}</Kbd>}
        <span className="decision-text"><span>{row.tone === 'deny' ? 'Deny…' : <CodeText text={row.label}/>}{row.choice && SCOPE_CHIP[row.choice.scope] && <em className="decision-scope">{SCOPE_CHIP[row.choice.scope]}</em>}</span>{row.detail && <small>{row.detail}</small>}</span>
        {row.key && keys && <Kbd>{row.key.toUpperCase()}</Kbd>}
      </button>)}
      {enableFullAccess && approval.command && !settings && !approval.acceptUnsupported && <button type="button" className="decision-row" data-tone="always" disabled={waiting} onClick={() => void decide(Decision.ACCEPT, { fullAccess: true })}>
        <span className="decision-text"><span>Allow and enable Full access</span><small>Approves this command and skips approvals for agent tools and ordinary Lazybox commands in this conversation. Codex runs with full filesystem access on the next turn.</small></span>
      </button>}
    </div> : <form className="deny-reason" onSubmit={event => { event.preventDefault(); deny(); }}>
      <label>Why not? <small>Optional. {agentName} is told, and can try another way.</small>
        <textarea rows={2} value={reason} placeholder={`e.g. Use make test instead`} onChange={e => setReason(e.target.value)} onKeyDown={event => {
          if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); deny(); }
          if (event.key === 'Escape') { event.preventDefault(); back(); }
        }}/>
      </label>
      <div className="row"><button type="button" disabled={waiting} onClick={back}>Back</button><button type="submit" className="danger" disabled={waiting}>Deny</button></div>
    </form>}
    {waiting && <small role="status" className="request-sending">Sending…</small>}
  </article>;
}

/* ───────────────────────── Questions ───────────────────────── */

type Answer = { picked: string[]; other: string; otherOn: boolean };
const empty: Answer = { picked: [], other: '', otherOn: false };
export function answerValues(q: Question, a: Answer = empty): string[] {
  if (!q.options.length) return a.other.trim() ? [a.other.trim()] : [];
  const other = a.otherOn && a.other.trim() ? [a.other.trim()] : [];
  return q.multi ? [...a.picked, ...other] : other.length ? other : a.picked.slice(0, 1);
}

/** Answers typed or picked, per request, kept while the card remounts (a refresh, the phone re-rendering its list). */
const answerDrafts = new Map<string, Record<string, Answer>>();
function QuestionCard({ request, api, session, onError, position }: RequestCardProps) {
  const data = useMemo(() => providerData(request.providerDataJson), [request.providerDataJson]);
  const questions = useMemo(() => questionsOf(data), [data]);
  const [answers, setAnswersState] = useState<Record<string, Answer>>(() => answerDrafts.get(request.requestId) || {});
  const setAnswers = (next: (all: Record<string, Answer>) => Record<string, Answer>) => setAnswersState(all => { const value = next(all); answerDrafts.set(request.requestId, value); return value; });
  const [waiting, setWaiting] = useState(false);
  const card = useRef<HTMLElement>(null);
  const complete = questions.length > 0 && questions.every(q => answerValues(q, answers[q.key]).length > 0);
  // One answer per request, however many keys asked for it in the same instant.
  const sent = useRef(false);
  const send = async () => {
    if (!complete || sent.current) return;
    sent.current = true; setWaiting(true);
    try { await api.apply({ sessionId: session, operationId: crypto.randomUUID(), operation: { case: 'answer', value: { requestId: request.requestId, answers: Object.fromEntries(questions.map(q => [q.key, { values: answerValues(q, answers[q.key]) }])) } } }); answerDrafts.delete(request.requestId); }
    catch (e) { onError(humanError(e)); setWaiting(false); sent.current = false; }
  };
  const update = (q: Question, change: (a: Answer) => Answer) => setAnswers(all => ({ ...all, [q.key]: change(all[q.key] || empty) }));
  /** Picks option `index` (options.length is "Other…"); true when that answers the question. */
  const pick = (q: Question, index: number): boolean => {
    if (index === q.options.length && q.other) {
      update(q, a => ({ ...a, otherOn: q.multi ? !a.otherOn : true, picked: q.multi ? a.picked : [] }));
      requestAnimationFrame(() => card.current?.querySelector<HTMLInputElement>(`[data-question="${CSS.escape(q.key)}"] .question-other input`)?.focus());
      return false;
    }
    const label = q.options[index]?.label;
    if (!label) return false;
    update(q, a => q.multi ? { ...a, picked: a.picked.includes(label) ? a.picked.filter(l => l !== label) : [...a.picked, label] } : { ...a, picked: [label], otherOn: false });
    return !q.multi;
  };
  /** After a single choice: the next unanswered question, or Send. */
  const advance = (from: Question) => requestAnimationFrame(() => {
    const node = card.current; if (!node) return;
    const next = questions.slice(questions.indexOf(from) + 1).find(q => q.options.length);
    (next ? node.querySelector<HTMLElement>(`[data-question="${CSS.escape(next.key)}"] [role=radio], [data-question="${CSS.escape(next.key)}"] [role=checkbox]`) : node.querySelector<HTMLElement>('.question-send'))?.focus();
  });
  const questionAt = (target: EventTarget | null) => {
    const key = target instanceof HTMLElement ? target.closest<HTMLElement>('[data-question]')?.dataset.question : undefined;
    return questions.find(q => q.key === key) || questions.find(q => !answerValues(q, answers[q.key]).length && q.options.length) || questions[0];
  };
  const onKey = (event: { key: string; target: EventTarget | null }) => {
    const q = questionAt(event.target);
    if (!q || !/^[1-9]$/.test(event.key)) return false;
    if (pick(q, Number(event.key) - 1)) advance(q);
    return true;
  };
  useChatKeys(card, onKey);
  // A question answered only in words starts in its field: nothing typed is lost, and typing answers nothing until it is sent.
  useInitialFocus(card, questions[0] && !questions[0].options.length ? '.question-free input, .question-free textarea' : undefined);
  const cardKeys = (event: ReactKeyboardEvent<HTMLElement>) => {
    if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') { event.preventDefault(); void send(); return; }
    if (editable(event.target) || event.metaKey || event.ctrlKey || event.altKey) return;
    // From the card itself, ↓ and ↑ go to the first and last option, as on the other cards.
    if (arrowIntoCard(event, card.current, '[role=radio], [role=checkbox]')) return;
    if (onKey(event)) event.preventDefault();
  };
  return <article ref={card} tabIndex={-1} className="request question-card" aria-label="Agent question" onKeyDown={cardKeys}>
    <CardHead eyebrow="Agent question" position={position}/>
    {!questions.length && <p className="approval-description">The agent asked a question Lazybox cannot read. Request details below.</p>}
    {questions.map((q, i) => <QuestionField key={q.key} question={q} index={i} total={questions.length} answer={answers[q.key] || empty} disabled={waiting}
      onPick={index => { if (pick(q, index)) advance(q); }} onOther={text => update(q, a => ({ ...a, other: text, otherOn: q.options.length ? (text ? true : a.otherOn) : false, picked: q.multi || !text ? a.picked : [] }))}
      onSubmit={() => void send()} complete={complete}/>)}
    {/* The raw request is a diagnostic: shown only for a question Lazybox could not read. */}
    {!questions.length && <details className="request-metadata"><summary>Request details</summary><CodeBlock text={JSON.stringify(data, null, 2)} language="json"/><small className="mono">{request.requestId}</small></details>}
    <footer className="question-footer">
      <small className="question-hint" aria-hidden="true">{questions.some(q => q.options.length) ? '1–9 to choose · ↑↓ to move · ' : ''}⌘↵ to send</small>
      <button type="button" className="primary question-send" disabled={waiting || !complete} onClick={() => void send()}>{waiting ? 'Sending…' : 'Send answer'}</button>
    </footer>
  </article>;
}

function QuestionField({ question: q, index, total, answer, disabled, onPick, onOther, onSubmit, complete }: { question: Question; index: number; total: number; answer: Answer; disabled: boolean; onPick(index: number): void; onOther(text: string): void; onSubmit(): void; complete: boolean }) {
  const [focused, setFocused] = useState(-1);
  const group = useRef<HTMLDivElement>(null);
  const id = `q-${index}-${q.key.length}`;
  const role = q.multi ? 'checkbox' : 'radio';
  const rows = q.options.length + (q.other && q.options.length ? 1 : 0);
  const selected = (i: number) => i === q.options.length ? answer.otherOn : answer.picked.includes(q.options[i].label);
  // One tab stop per question: the selected option, else the first.
  const tabStop = Math.max(0, [...Array(rows).keys()].find(selected) ?? 0);
  const preview = q.options.find((o, i) => o.preview && (i === focused || (focused < 0 && selected(i))))?.preview ?? (q.options.some(o => o.preview) ? q.options.find(o => o.preview)?.preview : undefined);
  const move = (event: ReactKeyboardEvent, i: number) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      group.current?.querySelectorAll<HTMLElement>(`[role=${role}]`)[(i + (event.key === 'ArrowDown' ? 1 : -1) + rows) % rows]?.focus();
    } else if (event.key === ' ' || (event.key === 'Enter' && !event.metaKey && !event.ctrlKey)) {
      event.preventDefault();
      // Enter on a chosen single answer, once everything is answered, sends.
      if (event.key === 'Enter' && !q.multi && selected(i) && complete) { onSubmit(); return; }
      onPick(i);
    }
  };
  if (!q.options.length) return <div className="question" data-question={q.key}>
    {q.header && <span className="question-header">{q.header}</span>}
    <label className="question-free"><span className="question-text">{q.text}</span>
      <input type={q.secret ? 'password' : 'text'} value={answer.other} disabled={disabled} autoComplete="off" placeholder="Your answer" onChange={e => onOther(e.target.value)} onKeyDown={event => { if (event.key === 'Enter' && !event.nativeEvent.isComposing) { event.preventDefault(); onSubmit(); } }}/>
    </label>
  </div>;
  return <div className="question" data-question={q.key} data-preview={preview !== undefined || undefined}>
    <div className="question-main">
      <div className="question-title">{q.header && <span className="question-header">{q.header}</span>}{total > 1 && <small>{index + 1}/{total}</small>}</div>
      <p className="question-text" id={id}>{q.text}{q.multi && <small> · choose any</small>}</p>
      <div ref={group} className="question-options" role={q.multi ? 'group' : 'radiogroup'} aria-labelledby={id}>
        {q.options.map((o, i) => <div key={o.label} role={role} aria-checked={selected(i)} aria-disabled={disabled || undefined} tabIndex={i === tabStop ? 0 : -1} className="question-option"
          onClick={() => !disabled && onPick(i)} onKeyDown={event => !disabled && move(event, i)} onFocus={() => setFocused(i)} onMouseEnter={() => setFocused(i)} onMouseLeave={() => setFocused(-1)}>
          <Kbd>{i + 1}</Kbd><span className="question-mark" data-multi={q.multi || undefined} aria-hidden="true"/>
          <span className="question-option-text"><span>{o.label}</span>{o.description && <small>{o.description}</small>}</span>
        </div>)}
        {q.other && <div role={role} aria-checked={answer.otherOn} tabIndex={tabStop === q.options.length ? 0 : -1} className="question-option question-other"
          onClick={event => { if (!disabled && !(event.target instanceof HTMLInputElement)) onPick(q.options.length); }} onKeyDown={event => { if (!disabled && !(event.target instanceof HTMLInputElement)) move(event, q.options.length); }} onFocus={() => setFocused(-1)}>
          <Kbd>{q.options.length + 1}</Kbd><span className="question-mark" data-multi={q.multi || undefined} aria-hidden="true"/>
          {answer.otherOn ? <input aria-label={`Your own answer to: ${q.text}`} value={answer.other} disabled={disabled} placeholder="Type your answer" onChange={e => onOther(e.target.value)} onKeyDown={event => {
            if (event.key === 'Enter' && !event.nativeEvent.isComposing) { event.preventDefault(); onSubmit(); }
            if (event.key === 'Escape') { event.preventDefault(); (event.currentTarget.closest('[role]') as HTMLElement | null)?.focus(); }
          }}/> : <span className="question-option-text"><span>Other…</span><small>Answer in your own words</small></span>}
        </div>}
      </div>
    </div>
    {preview !== undefined && <pre className="question-preview" aria-label="Preview">{preview}</pre>}
  </div>;
}

/* ───────────────────────── In the transcript ───────────────────────── */

/** How a request was answered: one quiet line where it was asked. */
export function AnsweredLine({ note }: { note: AnswerNote }) {
  if (note.kind === 'answered') return <div className="request-answered" role="note" data-kind="answered"><Icon name="check" size={13}/><span>{note.answers.map((a, i) => <span key={i}>{i > 0 && ' · '}{a.question}: <strong>{a.answer}</strong></span>)}</span></div>;
  if (note.kind === 'always') return <div className="request-answered" role="note" data-kind="always"><Icon name="shield" size={13}/><span>Always allowed <strong><CodeText text={note.label}/></strong>{note.scope ? ` ${scopeWords(note.scope)}` : ''}</span></div>;
  return <div className="request-answered" role="note" data-kind="declined"><Icon name="close" size={13}/><span>Denied {note.tool}{note.target ? <> · <code>{note.target.length > 80 ? note.target.slice(0, 79) + '…' : note.target}</code></> : null}{note.reason ? <> — “{note.reason}”</> : null}</span></div>;
}
