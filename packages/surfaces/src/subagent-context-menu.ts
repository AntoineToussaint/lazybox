/**
 * The context menu of a sub-agent or a background command (scope `subagent`,
 * see `ContextMenu.tsx`): on its card in the conversation, its chip in the
 * live bar and its row in Background work. The same actions those places
 * have, plus copying what it was asked, found or ran.
 */
import { copyText } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import { mergedStatus, type Subagent } from './subagent-model';
import type { SubagentView } from './subagent-view';

export type SubagentMenuDetail = { agentId: string; kind: 'agent' | 'command' };

export function subagentItems(view: SubagentView, agent: Subagent, title: string): MenuItem[] {
  const status = mergedStatus(agent, view.runner);
  const stop = view.canStopAgent(agent, status), stopping = view.isStopping(agent) && 'Stopping…';
  if (agent.kind === 'command') return [
    { label: 'Show output', icon: 'output', run: () => view.open(agent.id) },
    { label: 'Copy command', icon: 'copy', disabled: !agent.command && 'The agent did not say what it ran', run: () => copyText(agent.command, 'Copied command') },
    !!agent.output && { label: 'Copy output', icon: 'copy', run: () => copyText(agent.output, 'Copied output') },
    !!agent.outputFile && { label: 'Copy output path', icon: 'copy', run: () => copyText(agent.outputFile, 'Copied output path') },
    'separator',
    stop && { label: 'Stop background command', shortcut: 'Backspace', icon: 'stop', danger: true, disabled: stopping, run: () => view.stop(agent) },
  ];
  const expanded = view.expanded.has(agent.id);
  return [
    { label: expanded ? 'Hide its steps' : 'Show its steps', icon: expanded ? 'collapse' : 'expand', run: () => expanded ? view.toggle(agent.id, false) : view.reveal(agent.id) },
    { label: 'Open in a panel', icon: 'dock-right', run: () => view.open(agent.id) },
    'separator',
    !!agent.prompt && { label: 'Copy prompt', icon: 'copy', run: () => copyText(agent.prompt, 'Copied prompt') },
    !!agent.result && { label: 'Copy result', icon: 'copy', run: () => copyText(agent.result, 'Copied result') },
    !!title && { label: 'Copy name', icon: 'copy', run: () => copyText(title, 'Copied name') },
    'separator',
    stop && { label: 'Stop sub-agent', shortcut: 'Backspace', icon: 'stop', danger: true, disabled: stopping, run: () => view.stop(agent) },
  ];
}
