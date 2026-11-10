/** Persisted personal composition contains references, never credentials or operations. */
export type ViewTarget = { collectionId: string; sessionId: string; commandId: string };
export type Binding = { mode: 'follow' } | { mode: 'pinned'; target: ViewTarget };
export type Panel = { id: string; kind: string; binding: Binding; detached: boolean; collapsed?: boolean };
export type LayoutNode = { type: 'panel'; panel: Panel } | { type: 'split'; direction: 'row' | 'column'; ratio: number; children: [LayoutNode, LayoutNode] };
export type WorkspacePage = { id: string; name: string; selection: ViewTarget; layout: LayoutNode };
export type WorkspaceLayout = { version: 1; selectedPage: string; pages: WorkspacePage[] };
export const emptyTarget = (): ViewTarget => ({ collectionId: '', sessionId: '', commandId: '' });
export const moduleKinds = ['collections', 'chat', 'console', 'assistant', 'sessions'] as const;
const idPattern = /^[a-zA-Z0-9_-]{1,128}$/;
const kindPattern = /^[a-z][a-z0-9-]{0,63}$/;
const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === 'object' && !Array.isArray(value);
const keys = (value: Record<string, unknown>, allowed: string[]) => Object.keys(value).every(key => allowed.includes(key));
const id = (value: unknown, optional = false): value is string => typeof value === 'string' && ((optional && value === '') || idPattern.test(value));

export function validTarget(value: unknown): value is ViewTarget {
  return object(value) && keys(value, ['collectionId', 'sessionId', 'commandId']) && id(value.collectionId, true) && id(value.sessionId, true) && id(value.commandId, true) && (!value.commandId || !!value.sessionId);
}
export function panels(node: LayoutNode): Panel[] { return node.type === 'panel' ? [node.panel] : node.children.flatMap(panels); }
export function resolveTarget(panel: Panel, selection: ViewTarget): ViewTarget { return panel.binding.mode === 'pinned' ? panel.binding.target : selection; }

export function parseLayout(text: string): WorkspaceLayout {
  if (text.length > 128 << 10) throw new Error('Page layout exceeds 128 KiB');
  const value: unknown = JSON.parse(text);
  const pageIDs = new Set<string>(), panelIDs = new Set<string>();
  const node = (value: unknown, depth: number): boolean => {
    if (!object(value) || depth > 8) return false;
    if (value.type === 'split') return keys(value, ['type', 'direction', 'ratio', 'children']) && ['row', 'column'].includes(value.direction as string) && typeof value.ratio === 'number' && Number.isFinite(value.ratio) && value.ratio >= 0.15 && value.ratio <= 0.85 && Array.isArray(value.children) && value.children.length === 2 && value.children.every(child => node(child, depth + 1));
    if (value.type !== 'panel' || !keys(value, ['type', 'panel']) || !object(value.panel)) return false;
    const p = value.panel;
    if (!keys(p, ['id', 'kind', 'binding', 'detached', 'collapsed']) || !id(p.id) || panelIDs.has(p.id) || typeof p.kind !== 'string' || !kindPattern.test(p.kind) || typeof p.detached !== 'boolean' || (p.collapsed !== undefined && typeof p.collapsed !== 'boolean') || !object(p.binding)) return false;
    panelIDs.add(p.id);
    const bindingValid = p.binding.mode === 'follow' ? keys(p.binding, ['mode']) : p.binding.mode === 'pinned' && keys(p.binding, ['mode', 'target']) && validTarget(p.binding.target);
    return bindingValid && (!p.detached || (p.kind === 'chat' && p.binding.mode === 'pinned' && validTarget(p.binding.target) && !!p.binding.target.sessionId));
  };
  if (!object(value) || !keys(value, ['version', 'selectedPage', 'pages']) || value.version !== 1 || !id(value.selectedPage) || !Array.isArray(value.pages) || value.pages.length < 1 || value.pages.length > 8) throw new Error('Unsupported page layout');
  for (const page of value.pages) {
    if (!object(page) || !keys(page, ['id', 'name', 'selection', 'layout']) || !id(page.id) || pageIDs.has(page.id) || typeof page.name !== 'string' || !page.name.trim() || page.name.length > 64 || /[\u0000-\u001f]/.test(page.name) || !validTarget(page.selection) || !node(page.layout, 0)) throw new Error('Invalid page or panel configuration');
    pageIDs.add(page.id);
    if (panels(page.layout as LayoutNode).length > 12) throw new Error('At most 12 panels per page');
  }
  if (!pageIDs.has(value.selectedPage)) throw new Error('Selected page does not exist');
  return value as WorkspaceLayout;
}

const leaf = (id: string, kind: string, collapsed = false): LayoutNode => ({ type: 'panel', panel: { id, kind, binding: { mode: 'follow' }, detached: false, collapsed } });
export function defaultLayout(selection = emptyTarget()): WorkspaceLayout {
  const panel=(id:string,kind:string,collapsed=false)=>leaf(selection.collectionId ? `${selection.collectionId}-${id}` : id,kind,collapsed);
  return { version: 1, selectedPage: 'work', pages: [
    { id: 'work', name: 'Work', selection: { ...selection }, layout: { type: 'split', direction: 'row', ratio: 0.22, children: [panel('work-collections', 'collections'), { type: 'split', direction: 'column', ratio: 0.72, children: [panel('work-chat', 'chat'), panel('work-console', 'console', true)] }] } },
  ] };
}
export function mapPanels(node: LayoutNode, change: (panel: Panel) => Panel): LayoutNode {
  return node.type === 'panel' ? { ...node, panel: change(node.panel) } : { ...node, children: [mapPanels(node.children[0], change), mapPanels(node.children[1], change)] };
}
export function removePanel(node: LayoutNode, id: string): LayoutNode | null {
  if (node.type === 'panel') return node.panel.id === id ? null : node;
  const first = removePanel(node.children[0], id), second = removePanel(node.children[1], id);
  return first && second ? { ...node, children: [first, second] } : first || second;
}
export function movePanel(node: LayoutNode, id: string, step: -1 | 1): LayoutNode {
  const items = panels(node), index = items.findIndex(panel => panel.id === id), other = items[index + step];
  if (index < 0 || !other) return node;
  const current = items[index];
  return mapPanels(node, panel => panel.id === id ? other : panel.id === other.id ? current : panel);
}
export function clonePage(page: WorkspacePage, newID: () => string): WorkspacePage {
  return { ...page, id: newID(), name: `${page.name.slice(0, 59)} copy`, selection: { ...page.selection }, layout: mapPanels(page.layout, panel => ({ ...panel, id: newID(), detached: false, binding: panel.binding.mode === 'follow' ? { mode: 'follow' } : { mode: 'pinned', target: { ...panel.binding.target } } })) };
}

/** Detached windows remain in saved layout, but do not reserve an empty pane. */
export function visibleLayout(node:LayoutNode):LayoutNode|null{
 if(node.type==='panel')return node.panel.detached?null:node;
 const first=visibleLayout(node.children[0]),second=visibleLayout(node.children[1]);
 return first&&second?{...node,children:[first,second]}:first||second;
}

/** Remove the retired built-in Monitor page without resetting other saved pages. */
export function retireMonitor(layout: WorkspaceLayout): WorkspaceLayout {
 const pages=layout.pages.filter(page=>!(page.id==='monitor'&&page.name==='Monitor'));
 if(pages.length===layout.pages.length)return layout;
 if(!pages.length)return defaultLayout(layout.pages[0]?.selection);
 return {...layout,pages,selectedPage:pages.some(page=>page.id===layout.selectedPage)?layout.selectedPage:pages[0].id};
}
