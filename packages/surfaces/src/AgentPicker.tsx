import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { revealAcpAgents } from './AcpAgentsSettings';
import { actionEnabled, runAction } from './actions';
import { Icon } from './Icons';
import { agentChoices, agentTitle, isAcp, type AgentChoice } from './providers';
import { useRunner } from './RunnerContext';
import { useProviderAvailability } from './useProviderAvailability';
import './agent-picker.css';

/** ACP agents are a preview: tested against Lazybox's fixtures, not each real agent (ACP.md). */
export function PreviewTag() {
  return <span className="agent-preview" title="Preview: ACP agents are tested against Lazybox’s own fixtures; a real agent may differ">Preview</span>;
}

/**
 * The agent a new conversation runs on: Claude, Codex, and every ACP agent
 * the Runner can start (Gemini CLI, Goose…, ACP.md), each with its icon and
 * version. A select-only combobox (WAI-ARIA): focus stays on it, arrows move
 * through the list, Enter or Space picks, Escape closes, a letter jumps.
 * "Add an agent…" closes the dialog it is in and opens Settings → Agents.
 */
export function AgentPicker({ value, disabled, onChange }: { value: string; disabled?: boolean; onChange(provider: string): void }) {
  const { info } = useRunner();
  const { report } = useProviderAvailability(!!info);
  const profiles = info?.profiles || [];
  const choices = useMemo(() => agentChoices(profiles, report), [profiles, report]);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const box = useRef<HTMLDivElement>(null);
  const combo = useRef<HTMLButtonElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const id = useId();
  const current = choices.find(c => c.provider === value);
  const canAdd = actionEnabled('settings.open');
  const show = () => { if (disabled) return; setActive(Math.max(0, choices.findIndex(c => c.provider === value))); setOpen(true); };
  const close = () => setOpen(false);
  const pick = (choice: AgentChoice | undefined) => { if (!choice?.ready) return; onChange(choice.provider); close(); combo.current?.focus(); };
  useEffect(() => {
    if (!open) return;
    const away = (event: PointerEvent) => { if (!box.current?.contains(event.target as Node)) close(); };
    document.addEventListener('pointerdown', away);
    return () => document.removeEventListener('pointerdown', away);
  }, [open]);
  useEffect(() => { if (open) list.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: 'nearest' }); }, [active, open]);
  // The next agent that can be picked, by steps of `by` from `from`.
  const step = (from: number, by: number) => {
    for (let i = 1; i <= choices.length; i++) {
      const next = ((from + by * i) % choices.length + choices.length) % choices.length;
      if (choices[next].ready) return next;
    }
    return from;
  };
  const keys = (event: KeyboardEvent) => {
    const handled = () => { event.preventDefault(); event.stopPropagation(); };
    if (!open) {
      if (['ArrowDown', 'ArrowUp', 'Enter', ' '].includes(event.key)) { handled(); show(); }
      return;
    }
    switch (event.key) {
      case 'ArrowDown': handled(); setActive(a => step(a, 1)); return;
      case 'ArrowUp': handled(); setActive(a => step(a, -1)); return;
      case 'Home': handled(); setActive(step(-1, 1)); return;
      case 'End': handled(); setActive(step(choices.length, -1)); return;
      case 'Enter': case ' ': handled(); pick(choices[active]); return;
      case 'Escape': handled(); close(); return;
      case 'Tab': close(); return;
    }
    if (event.key.length === 1 && /\S/.test(event.key)) {
      const letter = event.key.toLowerCase();
      const starts = (c: AgentChoice) => c.ready && c.title.toLowerCase().startsWith(letter);
      const after = choices.findIndex((c, i) => i > active && starts(c));
      const next = after >= 0 ? after : choices.findIndex(starts);
      if (next >= 0) { handled(); setActive(next); }
    }
  };
  const firstAcp = choices.findIndex(c => c.acp);
  return <div ref={box} className="agent-picker">
    <span className="agent-picker-label" id={`${id}-label`}>Agent</span>
    <button ref={combo} type="button" role="combobox" className="agent-picker-button" disabled={disabled} aria-labelledby={`${id}-label`} aria-haspopup="listbox" aria-expanded={open} aria-controls={`${id}-list`}
      aria-activedescendant={open ? `${id}-option-${active}` : undefined} onClick={() => open ? close() : show()} onKeyDown={keys}>
      <Icon name={current?.icon || 'agent'} size={16}/>
      <span className="agent-picker-title">{current?.title || agentTitle(value, profiles)}</span>
      {current?.version && <small className="agent-picker-version">{current.version}</small>}
      {isAcp(value) && <PreviewTag/>}
      <Icon name="chevron" size={14}/>
    </button>
    {open && <div className="agent-picker-popover">
      <ul ref={list} id={`${id}-list`} role="listbox" aria-labelledby={`${id}-label`}>
        {choices.map((choice, index) => <li key={choice.provider} id={`${id}-option-${index}`} data-index={index} role="option" aria-selected={choice.provider === value} aria-disabled={!choice.ready || undefined}
          className={[index === active ? 'active' : '', index === firstAcp ? 'first-acp' : ''].filter(Boolean).join(' ') || undefined}
          onPointerMove={() => choice.ready && setActive(index)} onMouseDown={event => event.preventDefault()} onClick={() => pick(choice)}>
          {index === firstAcp && <span className="agent-picker-group" aria-hidden="true">ACP agents · Preview</span>}
          <span className="agent-picker-row">
            <Icon name={choice.icon} size={16}/>
            <span className="agent-picker-name">{choice.title}</span>
            {choice.version && <small>{choice.version}</small>}
            {choice.acp && <PreviewTag/>}
            {choice.provider === value && <span className="agent-picker-check"><Icon name="check" size={13}/></span>}
          </span>
          {choice.detail && <small className="agent-picker-detail">{choice.detail}</small>}
        </li>)}
      </ul>
      {canAdd && <button type="button" className="agent-picker-add" tabIndex={-1} onMouseDown={event => event.preventDefault()} onClick={() => { close(); combo.current?.closest('dialog')?.close(); runAction('settings.open', 'agents'); revealAcpAgents(); }}><Icon name="plus" size={13}/>Add an agent…</button>}
    </div>}
  </div>;
}
