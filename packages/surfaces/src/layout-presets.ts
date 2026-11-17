/**
 * Layout presets: arrangements of panel kinds that a page can take in one
 * step. Built-ins ship here; people save their own (by kind, so a saved layout
 * applies to any workspace). Applying one keeps every panel the page already
 * has of a kind it uses, so conversations, drafts and shells carry over.
 */
import { AGENTS_SHARE, groupID, isChatTab, normalize, resolveTarget, sameConversation, shownPanels, type Group, type LayoutNode, type Panel, type WorkspacePage } from './workspace-model';

export type TemplateNode = { type: 'group'; kinds: string[]; minimized?: boolean } | { type: 'split'; direction: 'row' | 'column'; sizes: number[]; children: TemplateNode[] };
export type LayoutPreset = { id: string; name: string; description: string; layout: TemplateNode };

const g = (...kinds: string[]): TemplateNode => ({ type: 'group', kinds });
const row = (sizes: number[], ...children: TemplateNode[]): TemplateNode => ({ type: 'split', direction: 'row', sizes, children });
const column = (sizes: number[], ...children: TemplateNode[]): TemplateNode => ({ type: 'split', direction: 'column', sizes, children });

/** Every preset's left column: Workspaces above Agents, both always in view. */
const sidebar = column([1 - AGENTS_SHARE, AGENTS_SHARE], g('collections'), g('agents'));

/**
 * Every panel kind a layout may place, as PanelRegistry's `builtinPanels`
 * declares them (scripts/action-manifest.test.ts keeps the two in step). The
 * Runner checks layout files against it (actions.manifest.json).
 */
export const layoutPanelKinds = ['collections', 'agents', 'chat', 'console', 'terminal', 'assistant', 'changes', 'usage', 'sessions', 'missions', 'pulls', 'processes', 'actions', 'preview', 'web', 'browser', 'files', 'editor', 'artifacts', 'extension'] as const;

export const DEFAULT_PRESET = 'chat-focus';
export const builtinPresets: readonly LayoutPreset[] = [
  { id: 'chat-focus', name: 'Chat focus', description: 'The conversation, with command output below when a command runs', layout: row([0.2, 0.8], sidebar, column([0.62, 0.38], g('chat'), { type: 'group', kinds: ['console'], minimized: true })) },
  { id: 'chat-changes', name: 'Chat + Changes', description: 'Talk on the left, review what changed on the right', layout: row([0.2, 0.44, 0.36], sidebar, g('chat'), g('changes')) },
  { id: 'chat-editor-terminal', name: 'Chat + Editor + Terminal', description: 'Code beside the conversation, a shell under it', layout: row([0.18, 0.34, 0.48], sidebar, g('chat'), column([0.64, 0.36], g('editor'), g('terminal'))) },
  { id: 'review', name: 'Review', description: 'Changes, the file, and the conversation side by side', layout: row([0.16, 0.32, 0.3, 0.22], sidebar, g('changes'), g('editor'), g('chat')) },
  { id: 'everything', name: 'Everything', description: 'Every working panel at once', layout: row([0.16, 0.34, 0.5], sidebar, column([0.7, 0.3], g('chat'), g('console')), column([0.62, 0.38], row([0.3, 0.7], g('files'), g('editor')), g('terminal', 'changes'))) },
];

/** The kinds a template places, in reading order. */
export function templateKinds(node: TemplateNode): string[] { return node.type === 'group' ? node.kinds : node.children.flatMap(templateKinds); }

/**
 * The page arranged as `template`. Panels the page has of a kind the template
 * uses keep their identity (and so their content); new ones get `newID`.
 * Panels the template does not use are closed; chats in their own window stay.
 */
export function applyTemplate(page: WorkspacePage, template: TemplateNode, newID: () => string): WorkspacePage {
  const pool = shownPanels(page).filter(panel => !panel.detached || panel.kind === 'chat');
  const used = new Set<string>(), groupIDs: string[] = [];
  const take = (kind: string): Panel => {
    // The chat on the selected conversation (or one following the page) is preferred, so it stays in view.
    const candidates = pool.filter(panel => panel.kind === kind && !used.has(panel.id));
    const shows = (value: Panel) => value.binding.mode === 'follow' || sameConversation(resolveTarget(value, page.selection), page.selection);
    const panel = candidates.find(value => value.kind !== 'chat' || (shows(value) && !value.detached)) || candidates.find(value => !value.detached) || { id: newID(), kind, binding: { mode: 'follow' as const }, detached: false };
    used.add(panel.id);
    return panel;
  };
  // The page's other conversation tabs stay open, as tabs of the template's first chat group.
  let tabsPlaced = false;
  const withTabs = (members: Panel[]): Panel[] => {
    if (tabsPlaced || !members.some(panel => panel.kind === 'chat')) return members;
    tabsPlaced = true;
    const rest = pool.filter(panel => isChatTab(panel) && !used.has(panel.id));
    rest.forEach(panel => used.add(panel.id));
    return [...members, ...rest];
  };
  const build = (node: TemplateNode): LayoutNode => {
    if (node.type === 'split') return { type: 'split', direction: node.direction, sizes: [...node.sizes], children: node.children.map(build) };
    const members = withTabs(node.kinds.map(take)), id = groupID(members[0].id, groupIDs);
    groupIDs.push(id);
    const group: Group = { type: 'group', id, panels: members, active: members[0].id };
    return node.minimized ? { ...group, minimized: true } : group;
  };
  return { ...page, layout: normalize(build(template)), floating: [], popouts: [] };
}

/** A page's docked arrangement as a template (kinds and shares). */
export function templateOf(page: WorkspacePage): TemplateNode | null {
  // A group's conversation tabs are one chat in the arrangement: the conversations are not its shape.
  const kinds = (panels: Panel[]) => panels.filter((panel, index) => panel.kind !== 'chat' || panels.findIndex(other => other.kind === 'chat') === index).map(panel => panel.kind);
  const visit = (node: LayoutNode): TemplateNode => node.type === 'group'
    ? { type: 'group', kinds: kinds(node.panels), ...(node.minimized ? { minimized: true } : {}) }
    : { type: 'split', direction: node.direction, sizes: node.sizes.map(size => Math.round(size * 100) / 100), children: node.children.map(visit) };
  return page.layout ? visit(page.layout) : null;
}
/** Two templates with the same panels in the same places (sizes aside). */
export function sameTemplate(a: TemplateNode | null, b: TemplateNode | null): boolean {
  const key = (node: TemplateNode | null): unknown => !node ? null : node.type === 'group' ? node.kinds : [node.direction, node.children.map(key)];
  return JSON.stringify(key(a)) === JSON.stringify(key(b));
}

/**
 * Where layouts were saved before they were files (POWER.md, "Config files");
 * the main window moves them into layouts/*.yaml once.
 */
export const SAVED_LAYOUTS_KEY = 'layout.saved:v1';
// Saved layouts are a personal preference, shared by every workspace.
const kindPattern = /^[a-z][a-z0-9-]{0,63}$/;
function validTemplate(value: unknown, depth = 0): value is TemplateNode {
  if (!value || typeof value !== 'object' || depth > 8) return false;
  const node = value as Record<string, unknown>;
  if (node.type === 'group') return Array.isArray(node.kinds) && node.kinds.length > 0 && node.kinds.length <= 16 && node.kinds.every(kind => typeof kind === 'string' && kindPattern.test(kind)) && (node.minimized === undefined || typeof node.minimized === 'boolean');
  return node.type === 'split' && (node.direction === 'row' || node.direction === 'column') && Array.isArray(node.children) && node.children.length >= 2 && node.children.length <= 16
    && Array.isArray(node.sizes) && node.sizes.length === node.children.length && node.sizes.every(size => typeof size === 'number' && size > 0 && size <= 1) && node.children.every(child => validTemplate(child, depth + 1));
}
export function parseSavedLayouts(text: string | null): LayoutPreset[] {
  try {
    const value: unknown = JSON.parse(text || '[]');
    if (!Array.isArray(value)) return [];
    return value.filter((item): item is LayoutPreset => !!item && typeof item === 'object' && typeof item.id === 'string' && /^[a-zA-Z0-9_-]{1,64}$/.test(item.id) && typeof item.name === 'string' && !!item.name.trim() && item.name.length <= 48 && validTemplate(item.layout))
      .slice(0, 24).map(item => ({ id: item.id, name: item.name, description: typeof item.description === 'string' ? item.description.slice(0, 120) : '', layout: stackTemplateSidebar(item.layout) }));
  } catch { return []; }
}
/** A saved layout keeps its arrangement, except that Agents tabbed with Workspaces now sits under them. */
export function stackTemplateSidebar(node: TemplateNode): TemplateNode {
  if (node.type === 'split') return { ...node, children: node.children.map(stackTemplateSidebar) };
  const both = node.kinds.includes('collections') && node.kinds.includes('agents') && node.kinds.every(kind => kind === 'collections' || kind === 'agents');
  return both ? sidebar : node;
}
