import { Fragment } from 'react';
import { Icon } from './Icons';
import { disabledReason, normalizeMenu, type MenuItem } from './context-menu-model';

/**
 * The same items a context menu shows, as the buttons of a ⋯ `ActionMenu`:
 * one list of actions serves both, so the two menus of a row never disagree.
 * Separators become nothing (the ⋯ menus set off their dangerous actions
 * themselves); submenus are flattened under a heading.
 */
export function ActionMenuItems({ items, icons = false }: { items: MenuItem[]; icons?: boolean }) {
  return <>{normalizeMenu(items).map((entry, index) => {
    if (entry.kind === 'separator') return null;
    const { action } = entry;
    if (entry.submenu) return <Fragment key={index}>
      <small className="action-menu-heading">{action.label}</small>
      <ActionMenuItems items={entry.submenu.map(child => child.kind === 'action' ? child.action : 'separator')} icons={icons}/>
    </Fragment>;
    return <button key={index} type="button" className={[action.className, action.danger && 'danger'].filter(Boolean).join(' ') || undefined}
      disabled={!!action.disabled} title={disabledReason(action)} aria-current={action.checked || undefined} onClick={() => void action.run?.()}>
      {icons && action.icon && <Icon name={action.icon}/>}
      {icons ? <span>{action.label}</span> : action.label}
      {action.note && <small>{action.note}</small>}
    </button>;
  })}</>;
}
