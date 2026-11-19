import { useState, type ReactNode } from 'react';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import { actionEnabled, runAction, useActionRevision } from './actions';
import type { MenuItem } from './context-menu-model';
import type { DockHandle, GroupView } from './DockLayout';
import { Icon } from './Icons';
import { builtinPresets, sameTemplate, templateKinds, type LayoutPreset, type TemplateNode } from './layout-presets';
import { firstProblem, type ConfigDoc } from './config-files';
import './config-files.css';
import type { PanelModule } from './PanelRegistry';
import { dockSides, type DockSide, type Panel } from './workspace-model';

/** Each kind's colour in a thumbnail, from the theme's tokens so it follows light and dark themes. */
const tint: Record<string, string> = { collections: 'var(--surface-active)', agents: 'var(--surface-active)', chat: 'var(--accent-bg-strong)', editor: 'var(--info-bg-strong)', files: 'var(--ok-bg-strong)', terminal: 'var(--ok-bg)', console: 'var(--warn-bg)', changes: 'var(--warn-bg-strong)' };
/** A small picture of an arrangement: one box per group, marked with its panel's icon. */
export function LayoutThumbnail({ layout, icons, width = 84, height = 54 }: { layout: TemplateNode | null; icons: Record<string, string | undefined>; width?: number; height?: number }) {
  const boxes: ReactNode[] = [];
  const visit = (node: TemplateNode, x: number, y: number, w: number, h: number, key: string) => {
    if (node.type === 'group') {
      const kind = node.kinds[0], height = node.minimized ? Math.min(h, 7) : h, top = node.minimized ? y + h - height : y;
      boxes.push(<g key={key}>
        <rect x={x + 1} y={top + 1} width={Math.max(1, w - 2)} height={Math.max(1, height - 2)} rx={2.5} fill={tint[kind] || 'var(--surface-strong)'} stroke="var(--line-bold)" strokeWidth={0.6}/>
        {node.kinds.length > 1 && <rect x={x + 3} y={top + 2.5} width={Math.min(w - 6, 10)} height={1.6} rx={0.8} fill="var(--accent)"/>}
        {!node.minimized && w >= 16 && h >= 16 && <svg x={x + w / 2 - 6} y={top + height / 2 - 6} width={12} height={12} viewBox="0 0 24 24" style={{ color: 'var(--text-soft)' }}><Icon name={icons[kind] || 'file'} size={24}/></svg>}
      </g>);
      return;
    }
    // Minimized groups take a sliver; the others share what is left.
    const fixed: number[] = node.children.map(child => node.direction === 'column' && child.type === 'group' && child.minimized ? 7 : 0);
    const flexible = node.sizes.reduce((sum, size, index) => fixed[index] ? sum : sum + size, 0) || 1;
    const extent = (node.direction === 'row' ? w : h) - fixed.reduce((sum, value) => sum + value, 0);
    let offset = 0;
    node.children.forEach((child, index) => {
      const span = fixed[index] || extent * node.sizes[index] / flexible;
      if (node.direction === 'row') visit(child, x + offset, y, span, h, `${key}-${index}`); else visit(child, x, y + offset, w, span, `${key}-${index}`);
      offset += span;
    });
  };
  if (layout) visit(layout, 0, 0, width, height, 'n');
  return <svg className="layout-thumbnail" width={width} height={height} viewBox={`0 0 ${width} ${height}`} aria-hidden="true">{boxes}</svg>;
}

/**
 * The workspace header's Layout menu: presets as pictures, your layouts (files
 * in layouts/, POWER.md "Config files"), save, copy and import, reset.
 */
export function LayoutMenu({ menu, current, saved, broken = [], folder = '', cycleKey, icons, disabled, onApply, onSave, onRename, onDelete, onReset, onCopy, onImport }: {
  menu: React.RefObject<HTMLDetailsElement | null>;
  current: TemplateNode | null;
  saved: LayoutPreset[];
  /** Layout files with a mistake: listed with their first problem, not offered. */
  broken?: ConfigDoc[];
  /** The layouts folder, shown so a person can find the files. */
  folder?: string;
  /** The key that steps through the layouts (⌘⇧L), as it reads. */
  cycleKey?: string;
  icons: Record<string, string | undefined>;
  disabled?: boolean;
  onApply(preset: LayoutPreset): void;
  onSave(name: string): void;
  onRename(id: string, name: string): void;
  onDelete(id: string): void;
  onReset(): void;
  /** Copies a layout's YAML: one of the list, or the arrangement shown. */
  onCopy?(preset?: LayoutPreset): void;
  onImport?(): void;
}) {
  const [naming, setNaming] = useState<{ id: string; name: string } | null>(null);
  const finish = (event: { preventDefault(): void }) => {
    event.preventDefault();
    if (!naming?.name.trim()) return;
    if (naming.id) onRename(naming.id, naming.name.trim()); else onSave(naming.name.trim());
    setNaming(null);
  };
  const tile = (preset: LayoutPreset, own: boolean) => <div key={preset.id} className="layout-tile" aria-current={sameTemplate(current, preset.layout) || undefined}>
    <button className="layout-apply" disabled={disabled} aria-label={`Apply layout ${preset.name}`} title={preset.description || templateKinds(preset.layout).join(', ')} onClick={() => onApply(preset)}>
      <LayoutThumbnail layout={preset.layout} icons={icons}/>
      <span>{preset.name}</span>
    </button>
    {own && <span className="layout-tile-actions">
      {onCopy && <button className="icon-button" aria-label={`Copy layout ${preset.name} as YAML`} title="Copy as YAML" onClick={() => onCopy(preset)}><Icon name="copy" size={12}/></button>}
      <button className="icon-button" data-keep-open aria-label={`Rename layout ${preset.name}`} title="Rename" onClick={() => setNaming({ id: preset.id, name: preset.name })}><Icon name="pencil" size={12}/></button>
      <button className="icon-button" data-keep-open aria-label={`Delete layout ${preset.name}`} title="Delete" onClick={() => onDelete(preset.id)}><Icon name="close" size={12}/></button>
    </span>}
  </div>;
  return <ActionMenu ref={menu} className="layout-menu" label="Layout" icon="layout" title="Layout: presets and saved arrangements" onToggle={open => { if (!open) setNaming(null); }}>
    <small className="action-menu-heading">Layouts{cycleKey && <kbd className="layout-cycle-key" title="Next layout">{cycleKey}</kbd>}</small>
    <div className="layout-grid">{builtinPresets.map(preset => tile(preset, false))}</div>
    {(saved.length > 0 || broken.length > 0) && <><small className="action-menu-heading">Yours</small><div className="layout-grid">{saved.map(preset => tile(preset, true))}</div></>}
    {broken.length > 0 && <ul className="layout-broken" aria-label="Layout files with problems">{broken.map(doc => <li key={doc.name} title={`${doc.path}\n${firstProblem(doc)}`}>
      <Icon name="alert" size={12}/><span><code>{doc.name.slice('layouts/'.length)}.yaml</code> {firstProblem(doc)}</span>
    </li>)}</ul>}
    {naming
      ? <form className="layout-name" onSubmit={finish}>
        <input autoFocus aria-label={naming.id ? 'New layout name' : 'Layout name'} maxLength={48} placeholder="Name this layout" value={naming.name} onChange={event => setNaming({ ...naming, name: event.target.value })} onKeyDown={event => { if (event.key === 'Escape') { event.stopPropagation(); event.preventDefault(); setNaming(null); } }}/>
        <button className="primary" data-keep-open type="submit" disabled={!naming.name.trim()}>{naming.id ? 'Rename' : 'Save'}</button>
      </form>
      : <button className="layout-action" data-keep-open disabled={disabled || !current} onClick={() => setNaming({ id: '', name: '' })}><Icon name="save"/><span>Save current layout as…</span></button>}
    {onCopy && <button className="layout-action" disabled={!current} onClick={() => onCopy()}><Icon name="copy"/><span>Copy current layout as YAML</span></button>}
    {onImport && <button className="layout-action" onClick={onImport}><Icon name="download"/><span>Import a layout…</span></button>}
    <button className="layout-action" disabled={disabled} onClick={onReset}><Icon name="refresh"/><span>Reset to default</span></button>
    {folder && <small className="layout-folder" title={folder}>Your layouts are files in <code>{folder.replace(/^.*?(\/\.config\/)/, '~$1')}</code></small>}
  </ActionMenu>;
}

/** Every kind of panel: shown ones are focused, hidden ones open where they last were docked. */
export function PanelsMenu({ registry, shown, beside, disabled, menu, onShow, onClose }: {
  registry: Readonly<Record<string, PanelModule>>;
  /** Kinds on the page, and where. */
  shown: Map<string, 'shown' | 'window'>;
  /** When set (⌘\ on a lone panel), the chosen panel opens beside this one. */
  beside?: string;
  disabled(kind: string): boolean;
  menu: React.RefObject<HTMLDetailsElement | null>;
  onShow(kind: string): void;
  onClose(): void;
}) {
  return <ActionMenu ref={menu} className="panels-menu" label="Panels" icon="panels" title="Panels: show, focus or reopen any panel" onToggle={open => { if (!open) onClose(); }}>
    <small className="action-menu-heading">{beside ? `Open beside ${beside}` : 'Panels'}</small>
    {Object.entries(registry).map(([kind, module]) => {
      const state = shown.get(kind);
      // A kind a page may hold several of (Web) always opens a new one.
      if (module.multiple) return <button key={kind} className="show-panel" disabled={disabled(kind)} aria-label={`New ${module.title} panel`} onClick={() => onShow(kind)}>
        <Icon name={module.icon || 'file'}/><span>New {module.title} panel</span>
      </button>;
      return <button key={kind} className="show-panel" disabled={disabled(kind)} aria-label={`${state ? 'Focus' : 'Show'} ${module.title}`} onClick={() => onShow(kind)}>
        <Icon name={module.icon || 'file'}/><span>{module.title}</span>{state && <small>{state === 'window' ? 'In window' : 'Shown'}</small>}
      </button>;
    })}
  </ActionMenu>;
}

export type GroupCommands = {
  title(panel: Panel): string;
  canClose(panel: Panel): boolean;
  close(panel: Panel): void;
  /** Chats open in a native window of their own; other panels pop out with their group. */
  openWindow(panel: Panel, handle: DockHandle): void;
  canOpenWindow(panel: Panel): boolean;
  minimize(groupID: string, minimized: boolean): void;
  dockTo(panel: Panel, side: DockSide): void;
  splitWith(groupID: string, panel: Panel, side: 'right' | 'below'): void;
  /** Chat: keep a preview tab open, or make a regular tab its group's preview tab. */
  togglePin?(panel: Panel): void;
  /** Chat: the conversation on a page of its own. */
  moveToNewPage?(panel: Panel): void;
  cloneConversation?(panel: Panel): (() => void) | undefined;
  /** Close the group's other tabs, or those to the right of this one. */
  closeTabs?(panel: Panel, which: 'others' | 'right'): void;
};
type Keys = { split: string; close: string; maximize: string; /** Put back in main window, as the person's keys show it (⇧⌘D). */ putBack?: string };
/** A group's menu: the ⋯ beside its tabs and a tab's context menu show the same items. Shortcuts are in `shortcut()` notation. */
export function groupMenuItems(group: GroupView, handle: DockHandle, commands: GroupCommands, keys: Keys): MenuItem[] {
  const panel = group.active;
  if (!panel) return [];
  const name = commands.title(panel), docked = group.location === 'grid';
  // Tabs that "Close other tabs" and "Close tabs to the right" would close (the sidebar's lists stay).
  const closable = (value: Panel) => value.id !== panel.id && commands.canClose(value) && value.kind !== 'collections' && value.kind !== 'agents';
  const index = group.panels.findIndex(value => value.id === panel.id), others = group.panels.filter(closable), right = group.panels.slice(index + 1).filter(closable);
  const clone = commands.cloneConversation?.(panel);
  const previewed = panel.preview || panel.binding.mode === 'follow';
  return [
    !!clone && { label: 'Clone conversation…', icon: 'plus', run: clone },
    { label: 'Split right', icon: 'split-right', shortcut: keys.split, run: () => commands.splitWith(group.id, panel, 'right') },
    { label: 'Split down', icon: 'split-down', run: () => commands.splitWith(group.id, panel, 'below') },
    docked && { label: group.maximized ? 'Restore layout' : 'Maximize', icon: group.maximized ? 'restore' : 'maximize', shortcut: keys.maximize, run: () => handle.toggleMaximize(panel.id) },
    docked && group.canMinimize && { label: group.minimized ? 'Restore' : 'Minimize', icon: group.minimized ? 'chevron' : 'minimize', run: () => commands.minimize(group.id, !group.minimized) },
    docked && { label: 'Float over the page', icon: 'float', run: () => handle.float(panel.id) },
    docked
      ? { label: 'Open in new window', icon: 'external', disabled: commands.canOpenWindow(panel) ? false : 'Not available here', run: () => commands.openWindow(panel, handle) }
      : group.location === 'popout'
        ? { label: 'Put back in main window', icon: 'put-back', run: () => handle.dockBack(group.id) }
        : { label: 'Dock into the page', icon: 'back', run: () => handle.dockBack(group.id) },
    !!commands.moveToNewPage && panel.kind === 'chat' && !panel.detached && { label: 'Move to new page', icon: 'panels', run: () => commands.moveToNewPage!(panel) },
    // A preview tab (italic) is kept open; a regular tab becomes the preview tab, which the next single click replaces.
    !!commands.togglePin && panel.kind === 'chat' && !panel.detached && { label: previewed ? 'Keep open' : 'Use as preview tab', icon: previewed ? 'pin' : 'target', run: () => commands.togglePin!(panel) },
    'separator',
    ...dockSides.map(side => ({ label: `Dock ${side}`, icon: `dock-${side}`, className: 'dock-option', run: () => commands.dockTo(panel, side) })),
    'separator',
    !!commands.closeTabs && others.length > 0 && { label: 'Close other tabs', icon: 'close', run: () => commands.closeTabs!(panel, 'others') },
    !!commands.closeTabs && others.length > 0 && { label: 'Close tabs to the right', icon: 'close', disabled: right.length ? false : 'No tab to its right', run: () => commands.closeTabs!(panel, 'right') },
    commands.canClose(panel) && { label: `Close ${name}`, icon: 'close', danger: true, shortcut: keys.close, run: () => commands.close(panel) },
  ];
}
/**
 * Right of a group's tabs: maximize, and the group menu. A group floating over
 * the page docks back with an arrow; a group in a window of its own leads with
 * **Put back in main window**, labelled, since that window has no other way back.
 */
export function GroupActions({ group, handle, commands, keys }: { group: GroupView; handle: DockHandle; commands: GroupCommands; keys: Keys }) {
  const panel = group.active;
  if (!panel) return null;
  const name = commands.title(panel);
  return <>
    {group.location === 'grid' && <button className="icon-button dock-maximize" aria-label={group.maximized ? 'Restore layout' : `Maximize ${name}`} title={group.maximized ? 'Restore layout' : 'Maximize · double-click a tab'} onClick={() => handle.toggleMaximize(panel.id)}><Icon name={group.maximized ? 'restore' : 'maximize'} size={14}/></button>}
    {group.location === 'floating' && <button className="icon-button dock-back" aria-label={`Dock ${name} into the page`} title="Dock into the page" onClick={() => handle.dockBack(group.id)}><Icon name="back" size={14}/></button>}
    {group.location === 'popout' && <button type="button" className="dock-put-back" title={`Put ${name} back where it was in the main window, and close this window${keys.putBack ? ` (${keys.putBack})` : ''}`} onClick={() => handle.dockBack(group.id)}><Icon name="put-back" size={13}/><span>Put back in main window</span></button>}
    <ActionMenu className="dock-group-menu" label={`${name} panel actions`}>
      <ActionMenuItems icons items={groupMenuItems(group, handle, commands, keys)}/>
    </ActionMenu>
  </>;
}

/** What a group's `+` offers: a new conversation, and every kind of panel opened in that group. */
export type AddCommands = {
  title(panel: Panel): string;
  /** A conversation started from here shows in this group. */
  newConversation(groupID: string): void;
  /** Why a conversation cannot start now, or '' when it can. */
  whyNoConversation(): string;
  /** `]] c`, as the person's keys show it. */
  newConversationKeys?: string;
  /** ⌘T (this menu for the active group), as the person's keys show it. */
  addKeys?: string;
  registry: Readonly<Record<string, PanelModule>>;
  /** Kinds the + does not offer (the sidebar's). */
  hidden: ReadonlySet<string>;
  /** Where a kind is now: in this group, elsewhere on the page, or not shown. */
  where(kind: string, groupID: string): 'here' | 'elsewhere' | 'none';
  disabled(kind: string): string;
  openHere(groupID: string, kind: string): void;
};
/**
 * The `+` right after a group's tabs (and `] ] t` for the active group): a new
 * conversation first, then any panel, opened here as this group's front tab.
 * A panel shown elsewhere moves here; Web opens a new one.
 */
export function GroupAdd({ group, commands }: { group: GroupView; commands: AddCommands }) {
  useActionRevision();
  const front = group.active ? commands.title(group.active) : 'this';
  const why = commands.whyNoConversation();
  const canReview = actionEnabled('review.start');
  return <ActionMenu className="dock-add-menu" label={`New tab in the ${front} group`} title={`New conversation, or open a panel here${commands.addKeys ? ` (${commands.addKeys})` : ''}`} summary={<Icon name="plus" size={14}/>}>
    <small className="action-menu-heading">New tab here</small>
    <button type="button" className="dock-add-primary" disabled={!!why} title={why || undefined} onClick={() => commands.newConversation(group.id)}>
      <Icon name="chat"/><span>New conversation…</span>{commands.newConversationKeys && <kbd>{commands.newConversationKeys}</kbd>}
    </button>
    {canReview && <>
      <small className="action-menu-heading">Conversation templates</small>
      <button type="button" disabled={!!why} title={why || 'Open a read-only review of the workspace changes beside the work'}
        onClick={() => runAction('review.start', 'code-review-v1', 'menu')}>
        <Icon name="review"/><span>Code review</span><small>Workspace changes</small>
      </button>
      <button type="button" disabled={!!why} title={why || 'Open a read-only defensive security review of the workspace changes'}
        onClick={() => runAction('review.start', 'security-review-v1', 'menu')}>
        <Icon name="review"/><span>Security review</span><small>Workspace changes</small>
      </button>
      <button type="button" disabled={!!why} title={why || 'Run independent acceptance checks and automatically send failures for repair'}
        onClick={() => runAction('review.start', 'behavior-verification-v1', 'menu')}>
        <Icon name="review"/><span>Behavior verification</span><small>Workspace changes</small>
      </button>
    </>}
    <hr/>
    {Object.entries(commands.registry).filter(([kind]) => !commands.hidden.has(kind)).map(([kind, module]) => {
      const where = module.multiple ? 'none' : commands.where(kind, group.id), reason = commands.disabled(kind);
      return <button key={kind} type="button" disabled={where === 'here' || !!reason} title={reason || undefined}
        aria-label={where === 'here' ? `${module.title} (already here)` : where === 'elsewhere' ? `Move ${module.title} here` : module.multiple ? `New ${module.title} panel here` : `Open ${module.title} here`}
        onClick={() => commands.openHere(group.id, kind)}>
        <Icon name={module.icon || 'file'}/><span>{module.multiple ? `New ${module.title} panel` : module.title}</span>
        {where !== 'none' && <small>{where === 'here' ? 'Here' : 'Move here'}</small>}
      </button>;
    })}
  </ActionMenu>;
}
