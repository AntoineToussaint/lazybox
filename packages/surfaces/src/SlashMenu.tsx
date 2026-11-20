import { Fragment, useLayoutEffect, useRef } from 'react';
import { Icon } from './Icons';
import { highlight } from './QuickOpen';
import { SLASH_GROUPS, type SlashChoice, type SlashCommand, type SlashGroup, type SlashMenuState } from './slash-commands';
import './slash-menu.css';

export const SLASH_MENU_ID = (sessionId: string) => `slash-menu-${sessionId || 'new'}`;
export const slashOptionId = (sessionId: string, index: number) => `${SLASH_MENU_ID(sessionId)}-${index}`;

/**
 * The slash-command menu above the message box: the commands a typed name
 * matches, grouped (Lazybox · the agent · snippets), or the choices of the
 * command's argument (a model, a permission mode), or what a command takes.
 * The keyboard stays in the message box (↑/↓, Tab, Enter, Esc); the pointer
 * works too. Nothing here sends or runs: the owner does, on choose.
 */
export function SlashMenu({ menu, selected, sessionId, agentName, onSelect, onChoose, onClose }: {
  menu: SlashMenuState;
  selected: number;
  sessionId: string;
  agentName: string;
  onSelect(index: number): void;
  onChoose(index: number): void;
  onClose(): void;
}) {
  const list = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => { list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' }); }, [selected, menu]);
  const id = SLASH_MENU_ID(sessionId);
  const groupLabel: Record<SlashGroup, string> = { lazybox: 'Lazybox', provider: agentName, snippet: 'Snippets' };
  // Keep focus in the message box: a click chooses without taking it.
  const keep = (event: React.MouseEvent) => event.preventDefault();

  if (menu.kind === 'hint') {
    const { command } = menu;
    return <div className="slash-menu slash-menu-hint" id={id} role="status" onMouseDown={keep}>
      <div className="slash-hint-row">
        <span className="slash-icon"><Icon name={command.icon} size={15}/></span>
        <code className="slash-name">/{command.name}</code>
        {command.argumentHint && <code className="slash-args">{command.argumentHint}</code>}
        <span className="slash-description">{command.disabled || command.description}</span>
      </div>
      <Footer hint={command.disabled ? 'Esc to close' : command.group === 'provider' ? `Enter sends it to ${agentName}` : 'Enter to run · Esc to close'}/>
    </div>;
  }

  if (menu.kind === 'choices') {
    const { command, items } = menu;
    return <div className="slash-menu" onMouseDown={keep}>
      <div className="slash-group-label"><code>/{command.name}</code> {command.description}</div>
      <div className="slash-list" ref={list} id={id} role="listbox" aria-label={`/${command.name} choices`}>
        {items.map((choice, i) => <ChoiceRow key={choice.value} choice={choice} id={slashOptionId(sessionId, i)} selected={i === selected} onHover={() => onSelect(i)} onChoose={() => onChoose(i)}/>)}
      </div>
      <Footer hint={items[selected]?.disabled ? "Unavailable · ↑↓ to choose · Tab to leave · Esc to close" : command.freeArgument ? '↑↓ to choose · Enter to switch · or type an id · Esc to close' : '↑↓ to choose · Enter to apply · Esc to close'} onClose={onClose}/>
    </div>;
  }

  const unavailable = !!menu.items[selected]?.command.disabled;
  const groups = SLASH_GROUPS.map(group => ({ group, rows: menu.items.map((item, index) => ({ item, index })).filter(row => row.item.command.group === group) })).filter(g => g.rows.length);
  return <div className="slash-menu" onMouseDown={keep}>
    <div className="slash-list" ref={list} id={id} role="listbox" aria-label="Commands">
      {groups.map(({ group, rows }) => <Fragment key={group}>
        <div className="slash-group-label" role="presentation">{groupLabel[group]}</div>
        {rows.map(({ item, index }) => <CommandRow key={item.command.key} command={item.command} positions={item.positions} id={slashOptionId(sessionId, index)} selected={index === selected} onHover={() => onSelect(index)} onChoose={() => onChoose(index)}/>)}
      </Fragment>)}
    </div>
    <Footer hint={unavailable ? "Unavailable · Tab to leave · Esc to close" : "↑↓ to choose · Tab to complete · Enter to run · Esc to close"} onClose={onClose}/>
  </div>;
}

function CommandRow({ command, positions, id, selected, onHover, onChoose }: { command: SlashCommand; positions: number[]; id: string; selected: boolean; onHover(): void; onChoose(): void }) {
  return <div className="slash-item" id={id} role="option" aria-selected={selected} aria-disabled={command.disabled ? true : undefined} data-group={command.group}
    title={command.disabled} onMouseMove={selected ? undefined : onHover} onClick={onChoose}>
    <span className="slash-icon"><Icon name={command.icon} size={15}/></span>
    <code className="slash-name">/{highlight(command.name, positions)}</code>
    {command.argumentHint && <code className="slash-args">{command.argumentHint}</code>}
    {command.badge && <span className="slash-badge">{command.badge}</span>}
    <span className="slash-description">{command.disabled || command.description}</span>
  </div>;
}

function ChoiceRow({ choice, id, selected, onHover, onChoose }: { choice: SlashChoice; id: string; selected: boolean; onHover(): void; onChoose(): void }) {
  return <div className="slash-item slash-choice" id={id} role="option" aria-selected={selected} aria-disabled={choice.disabled ? true : undefined} aria-current={choice.current || undefined}
    title={choice.disabled} onMouseMove={selected ? undefined : onHover} onClick={onChoose}>
    <span className="slash-icon">{choice.current ? <Icon name="check" size={14}/> : null}</span>
    <span className="slash-choice-label">{choice.label}</span>
    {choice.current && <span className="slash-badge">Current</span>}
    <span className="slash-description">{choice.disabled || choice.detail}</span>
  </div>;
}

function Footer({ hint, onClose }: { hint: string; onClose?(): void }) {
  return <div className="slash-footer"><span>{hint}</span>{onClose && <button type="button" className="slash-close" aria-label="Close commands" onClick={onClose}><Icon name="close" size={12}/></button>}</div>;
}
