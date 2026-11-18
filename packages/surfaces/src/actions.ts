import { useEffect, useRef, useSyncExternalStore } from 'react';
import type { MenuAction } from './context-menu-model';

/**
 * The action registry: every command of the app, declared once.
 *
 * A definition says what an action is (title, group, icon, default keys,
 * where it can fire). Its behaviour comes from whoever is mounted for it: a
 * component registers a handler with `useAction`, optionally with a
 * when-enabled predicate. The keymap, the `]]` which-key menu, the command
 * palette, the shortcuts overlay, the macOS menu bar and context menus all
 * read this one list, so they cannot drift apart.
 *
 * Keys are written as KeyboardEvent names joined by `+` for a chord
 * (`Meta+k`, `Ctrl+Shift+p`), spaces between the keys of a sequence
 * (`] ] i`), and ` or ` between alternatives (`Meta+k or ] ] p`). `Mod+` in a
 * default is ⌘ on a Mac and Ctrl elsewhere; saved keymaps hold the resolved key.
 */
export type ActionScope = 'global' | 'navigation' | 'agent';
export type ActionGroup = 'General' | 'Go to' | 'Workspace' | 'Layout' | 'Conversation' | 'Voice' | 'Sidebar' | 'Message box';
export type MenuSection = 'File' | 'Go' | 'Window' | 'Help';
export type ActionDefinition = {
  id: string;
  title: string;
  group: ActionGroup;
  icon: string;
  /** Default keys, before `Mod+` is resolved for the platform; '' for none. */
  keys: string;
  scope: ActionScope;
  /** Extra words the palette matches ("cheatsheet" finds the shortcuts). */
  keywords?: string;
  /** Listed in the command palette (default true). */
  palette?: boolean;
  /** The native menu (desktop) lists it under this menu, with this title. */
  menu?: { section: MenuSection; title: string; separatorBefore?: boolean };
  /** Runs even while a dialog is open (it opens or closes one of its own). */
  overDialogs?: boolean;
  /** The settings refuse an empty shortcut. */
  required?: boolean;
  /** Members of a numbered series ("Go to workspace 1…9") share this. */
  series?: string;
  /**
   * A key held down (push-to-talk): the voice layer owns its keydown and
   * keyup, so the chord and sequence dispatchers leave it alone. Its keys
   * must be one chord with a modifier, a function key, or a right-hand
   * modifier alone (`AltRight`, `MetaRight`: held alone to talk, tapped
   * twice to keep listening).
   */
  hold?: boolean;
  /** Cannot be undone: voice asks before running it (VOICE.md). */
  destructive?: boolean;
  /** The default keys on Windows and Linux, where they differ from `keys` (a Mac's ⌃ and ⌘ are both taken there by Ctrl). */
  otherKeys?: string;
  /**
   * Cannot be rebound: the way back from any keymap and any layout. Its keys
   * are always its defaults, and no other action may take them.
   */
  fixed?: boolean;
  /** Held down, the key runs it again and again (resizing); other chords run once per press. */
  repeat?: boolean;
  /**
   * A hook may run it (`lazybox do` with a hook's `respond` credential,
   * POWER.md): it only shows, focuses or arranges what is on screen. Nothing
   * that changes a setting, starts or stops work, deletes, or needs an
   * approval is safe.
   */
  safe?: boolean;
  /** What the action's one argument is, when it takes one (`lazybox do ID ARG`, a `do:` step). */
  argument?: string;
};

const workspaceSlots = Array.from({ length: 9 }, (_, i) => i + 1);
/** Where a direction's arrow is. */
export type LayoutDirection = 'left' | 'right' | 'up' | 'down';
export const layoutDirections: readonly LayoutDirection[] = ['left', 'right', 'up', 'down'];

const head = [
  { id: 'palette.open', title: 'Command palette', group: 'General', icon: 'search', keys: 'Mod+k or Mod+Shift+p or ] ] p', scope: 'global', argument: 'text to search for', palette: false, overDialogs: true, keywords: 'commands actions search run', menu: { section: 'Go', title: 'Command Palette…' } },
  { id: 'shortcuts.open', title: 'Keyboard shortcuts', group: 'General', icon: 'keyboard', keys: '? or Mod+/ or ] ] ?', scope: 'global', safe: true, overDialogs: true, keywords: 'keys keymap help cheatsheet bindings', menu: { section: 'Help', title: 'Keyboard Shortcuts' } },
  { id: 'settings.open', title: 'Open settings', group: 'General', icon: 'sliders', keys: 'Mod+,', scope: 'global', argument: 'a section: appearance, chat, notifications, keyboard, power, …', keywords: 'preferences options configure' },
  { id: 'appearance.toggle', title: 'Switch between light and dark', group: 'General', icon: 'contrast', keys: '', scope: 'global', keywords: 'theme dark light mode appearance colour color' },
  { id: 'inbox.open', title: 'Open the inbox', group: 'General', icon: 'inbox', keys: '] ] i', scope: 'global', safe: true, keywords: 'attention notifications waiting', menu: { section: 'Go', title: 'Inbox' } },
  { id: 'attention.next', title: 'Next conversation that needs you', group: 'Go to', icon: 'bell', keys: 'Mod+j or ] ] n', scope: 'global', safe: true, keywords: 'agent waiting approval question failed review jump', menu: { section: 'Go', title: 'Next Conversation That Needs You', separatorBefore: true } },
  { id: 'attention.previous', title: 'Previous conversation that needs you', group: 'Go to', icon: 'bell', keys: 'Mod+Shift+j', scope: 'global', safe: true, keywords: 'agent waiting approval question failed review jump back', menu: { section: 'Go', title: 'Previous Conversation That Needs You' } },
  { id: 'conversation.next', title: 'Next conversation in this workspace', group: 'Go to', icon: 'chevronRight', keys: 'Mod+]', scope: 'global', safe: true, keywords: 'agent down', menu: { section: 'Go', title: 'Next Conversation' } },
  { id: 'conversation.previous', title: 'Previous conversation in this workspace', group: 'Go to', icon: 'back', keys: 'Mod+[', scope: 'global', safe: true, keywords: 'agent up', menu: { section: 'Go', title: 'Previous Conversation' } },
] as const satisfies readonly ActionDefinition[];
type Slot = 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9;
const slotDefinitions = workspaceSlots.map((n): ActionDefinition & { id: `workspace.select.${Slot}` } => ({ id: `workspace.select.${n as Slot}`, title: `Go to workspace ${n}`, group: 'Go to', icon: 'collections', keys: `Mod+${n}`, scope: 'global', palette: false, series: 'workspace.select' }));
const tail = [
  { id: 'files.find', title: 'Go to file', group: 'Go to', icon: 'search', keys: '] ] f or Mod+p', scope: 'global', safe: true, keywords: 'open file quick', menu: { section: 'Go', title: 'Go to File…', separatorBefore: true } },
  { id: 'navigation.exit', title: 'Focus the sidebar', group: 'Go to', icon: 'sidebar', keys: '] ]', scope: 'global', required: true, keywords: 'leave agent navigation leader' },
  // One sheet: repositories, branch, agent, mode and the first message; Enter starts it all (NewTaskSheet.tsx).
  { id: 'task.new', title: 'New task', group: 'Workspace', icon: 'agent', keys: 'Mod+n', scope: 'global', overDialogs: true, keywords: 'new workspace conversation agent prompt start repository branch create begin work', menu: { section: 'File', title: 'New Task…' } },
  { id: 'task.fromIssue', title: 'Start from an issue…', group: 'Workspace', icon: 'issue', keys: '', scope: 'global', overDialogs: true, keywords: 'new task workspace github issue ticket item link pull request pick search assigned' },
  { id: 'workspace.new', title: 'New workspace', group: 'Workspace', icon: 'plus', keys: '] ] w', scope: 'global', keywords: 'create add repositories', menu: { section: 'File', title: 'New Workspace…' } },
  { id: 'conversation.new', title: 'New conversation', group: 'Workspace', icon: 'chat', keys: 'Mod+Shift+n or ] ] c', scope: 'global', keywords: 'start agent session chat in this workspace', menu: { section: 'File', title: 'New Conversation…' } },
  { id: 'changes.show', title: 'Review changes', group: 'Workspace', icon: 'branch', keys: '] ] d', scope: 'global', safe: true, keywords: 'diff git commit pull request' },
  // Another agent reviews the workspace's changes, read-only (ReviewActions.tsx): the default reviewer, or one named.
  { id: 'review.start', title: 'Review this workspace’s changes', group: 'Workspace', icon: 'review', keys: '] ] Shift+r', scope: 'global', keywords: 'agent cross review findings second opinion codex claude default reviewer /review' },
  { id: 'review.codex', title: 'Review changes with Codex', group: 'Workspace', icon: 'review', keys: '', scope: 'global', keywords: 'agent cross review findings second opinion codex' },
  { id: 'review.claude', title: 'Review changes with Claude', group: 'Workspace', icon: 'review', keys: '', scope: 'global', keywords: 'agent cross review findings second opinion claude' },
  // The review under way stops: its reviewer interrupted and ended, no further round; its findings kept (ReviewCancel.tsx).
  { id: 'review.cancel', title: 'Cancel review', group: 'Workspace', icon: 'stop', keys: '', scope: 'global', keywords: 'stop abort end agent cross review reviewer codex claude loop round' },
  { id: 'files.explorer', title: 'Browse files', group: 'Workspace', icon: 'folder', keys: '] ] e', scope: 'global', safe: true, keywords: 'explorer tree' },
  { id: 'terminal.open', title: 'Open a terminal', group: 'Workspace', icon: 'terminal', keys: '', scope: 'global', keywords: 'shell console' },
  // The person's own terminal app at the workspace's folder (handoff.tsx); a browser copies the cd.
  { id: 'terminal.external', title: 'Open in Terminal', group: 'Workspace', icon: 'terminal', keys: '', scope: 'global', keywords: 'your own terminal app iterm ghostty wezterm warp cd folder worktree external shell lazybox run' },
  { id: 'artifacts.open', title: 'Show artifacts', group: 'Workspace', icon: 'artifact', keys: '] ] a', scope: 'global', safe: true, keywords: 'documents reports' },
  { id: 'web.open', title: 'Open a web page', group: 'Workspace', icon: 'globe', keys: '] ] b', scope: 'global', keywords: 'browser web viewer url link site preview', menu: { section: 'File', title: 'New Web Panel' } },
  { id: 'console.toggle', title: 'Show or hide command output', group: 'Workspace', icon: 'output', keys: '] ] o', scope: 'global', safe: true, keywords: 'console close open log run tests' },
  { id: 'workspace.window', title: 'Open this workspace in a new window', group: 'Workspace', icon: 'external', keys: '', scope: 'global', keywords: 'detach' },
  { id: 'page.customize', title: 'Customize this page', group: 'Workspace', icon: 'pencil', keys: '', scope: 'global', keywords: 'layout panels edit arrange' },
  { id: 'layout.configure', title: 'Layout configuration', group: 'Workspace', icon: 'sliders', keys: '', scope: 'global', keywords: 'json import export' },
  // The page's dock (DockLayout): the active panel is the one last clicked or focused.
  { id: 'layout.split', title: 'Split the active panel', group: 'Layout', icon: 'split-right', keys: 'Mod+\\', scope: 'global', keywords: 'dock pane beside side by side' },
  { id: 'layout.split-down', title: 'Split the active panel down', group: 'Layout', icon: 'split-down', keys: '', scope: 'global', keywords: 'dock pane below stacked' },
  { id: 'layout.close', title: 'Close the active panel', group: 'Layout', icon: 'close', keys: 'Mod+w', scope: 'global', keywords: 'dock pane hide' },
  { id: 'layout.maximize', title: 'Maximize or restore the active panel', group: 'Layout', icon: 'maximize', keys: 'Mod+Shift+m or ] ] m', scope: 'global', safe: true, keywords: 'zoom dock pane full screen' },
  // Every tab of the page in turn: the active group's, then the next group's (a group of one tab is one step, as before).
  { id: 'layout.next-group', title: 'Next tab or panel group', group: 'Layout', icon: 'chevronRight', keys: 'Ctrl+Tab', scope: 'global', safe: true, keywords: 'dock pane focus cycle conversation tab' },
  { id: 'layout.previous-group', title: 'Previous tab or panel group', group: 'Layout', icon: 'back', keys: 'Ctrl+Shift+Tab', scope: 'global', safe: true, keywords: 'dock pane focus cycle conversation tab' },
  { id: 'layout.reopen-tab', title: 'Reopen the last closed conversation tab', group: 'Layout', icon: 'refresh', keys: 'Mod+Shift+t', scope: 'global', keywords: 'undo close restore conversation chat tab back' },
  { id: 'layout.float', title: 'Float the active panel over the page', group: 'Layout', icon: 'float', keys: '', scope: 'global', keywords: 'dock pane window overlay' },
  { id: 'layout.window', title: 'Open the active panel in a new window', group: 'Layout', icon: 'external', keys: '', scope: 'global', keywords: 'dock pane pop out detach second screen' },
  // Panels in windows of their own (a chat's window, a popped-out group): back where they came from.
  { id: 'window.put-back', title: 'Put back in main window', group: 'Layout', icon: 'put-back', keys: 'Mod+Shift+d', scope: 'global', keywords: 'dock attach reattach return window detached popout pop in bring back', menu: { section: 'Window', title: 'Put Back in Main Window' } },
  { id: 'window.put-all-back', title: 'Put all windows back', group: 'Layout', icon: 'put-back', keys: '', scope: 'global', keywords: 'dock attach reattach return windows detached popouts bring back every' },
  { id: 'layout.minimize', title: 'Minimize or restore the active panel group', group: 'Layout', icon: 'minimize', keys: '', scope: 'global', safe: true, keywords: 'dock pane collapse' },
  { id: 'layout.panels', title: 'Show a panel…', group: 'Layout', icon: 'panels', keys: '', scope: 'global', safe: true, keywords: 'dock add open reopen panel' },
  { id: 'layout.presets', title: 'Change the layout…', group: 'Layout', icon: 'layout', keys: '', scope: 'global', safe: true, keywords: 'dock preset arrangement save' },
  // Layouts as files (POWER.md, "Config files"): the presets, then the person's layouts/*.yaml.
  { id: 'layout.cycle', title: 'Next layout', group: 'Layout', icon: 'layout', keys: 'Mod+Shift+l', scope: 'global', safe: true, keywords: 'dock preset arrangement cycle switch rotate' },
  { id: 'layout.copy', title: 'Copy the layout as YAML', group: 'Layout', icon: 'copy', keys: '', scope: 'global', keywords: 'dock preset arrangement share export clipboard file yaml' },
  { id: 'layout.import', title: 'Import a layout…', group: 'Layout', icon: 'download', keys: '', scope: 'global', keywords: 'dock preset arrangement share paste clipboard file yaml' },
  { id: 'layout.reset', title: 'Reset the layout to default', group: 'Layout', icon: 'refresh', keys: '', scope: 'global', keywords: 'dock preset chat focus' },
  // Panel groups from the keyboard (DockLayout, dock-keyboard.ts). A Mac's ⌘ and ⌃ are both Ctrl
  // elsewhere, so Windows and Linux get keys of their own (`otherKeys`); every one is rebindable.
  { id: 'layout.focus-left', title: 'Focus the panel group to the left', group: 'Layout', icon: 'back', keys: 'Meta+Alt+ArrowLeft', otherKeys: 'Ctrl+Alt+ArrowLeft', scope: 'global', safe: true, keywords: 'dock pane go navigate neighbour west' },
  { id: 'layout.focus-right', title: 'Focus the panel group to the right', group: 'Layout', icon: 'chevronRight', keys: 'Meta+Alt+ArrowRight', otherKeys: 'Ctrl+Alt+ArrowRight', scope: 'global', safe: true, keywords: 'dock pane go navigate neighbour east' },
  { id: 'layout.focus-up', title: 'Focus the panel group above', group: 'Layout', icon: 'up', keys: 'Meta+Alt+ArrowUp', otherKeys: 'Ctrl+Alt+ArrowUp', scope: 'global', safe: true, keywords: 'dock pane go navigate neighbour north' },
  { id: 'layout.focus-down', title: 'Focus the panel group below', group: 'Layout', icon: 'chevron', keys: 'Meta+Alt+ArrowDown', otherKeys: 'Ctrl+Alt+ArrowDown', scope: 'global', safe: true, keywords: 'dock pane go navigate neighbour south' },
  { id: 'layout.move-left', title: 'Move the active panel left', group: 'Layout', icon: 'dock-left', keys: 'Meta+Alt+Shift+ArrowLeft', otherKeys: 'Ctrl+Alt+Shift+ArrowLeft', scope: 'global', keywords: 'dock pane swap neighbour group tab' },
  { id: 'layout.move-right', title: 'Move the active panel right', group: 'Layout', icon: 'dock-right', keys: 'Meta+Alt+Shift+ArrowRight', otherKeys: 'Ctrl+Alt+Shift+ArrowRight', scope: 'global', keywords: 'dock pane swap neighbour group tab' },
  { id: 'layout.move-up', title: 'Move the active panel up', group: 'Layout', icon: 'dock-top', keys: 'Meta+Alt+Shift+ArrowUp', otherKeys: 'Ctrl+Alt+Shift+ArrowUp', scope: 'global', keywords: 'dock pane swap neighbour group tab' },
  { id: 'layout.move-down', title: 'Move the active panel down', group: 'Layout', icon: 'dock-bottom', keys: 'Meta+Alt+Shift+ArrowDown', otherKeys: 'Ctrl+Alt+Shift+ArrowDown', scope: 'global', keywords: 'dock pane swap neighbour group tab' },
  { id: 'layout.resize-left', title: 'Resize: move the active group’s edge left', group: 'Layout', icon: 'back', keys: 'Ctrl+Meta+ArrowLeft', otherKeys: 'Alt+Shift+ArrowLeft', scope: 'global', repeat: true, keywords: 'dock pane size grow shrink wider narrower divider' },
  { id: 'layout.resize-right', title: 'Resize: move the active group’s edge right', group: 'Layout', icon: 'chevronRight', keys: 'Ctrl+Meta+ArrowRight', otherKeys: 'Alt+Shift+ArrowRight', scope: 'global', repeat: true, keywords: 'dock pane size grow shrink wider narrower divider' },
  { id: 'layout.resize-up', title: 'Resize: move the active group’s edge up', group: 'Layout', icon: 'up', keys: 'Ctrl+Meta+ArrowUp', otherKeys: 'Alt+Shift+ArrowUp', scope: 'global', repeat: true, keywords: 'dock pane size grow shrink taller shorter divider' },
  { id: 'layout.resize-down', title: 'Resize: move the active group’s edge down', group: 'Layout', icon: 'chevron', keys: 'Ctrl+Meta+ArrowDown', otherKeys: 'Alt+Shift+ArrowDown', scope: 'global', repeat: true, keywords: 'dock pane size grow shrink taller shorter divider' },
  { id: 'layout.add', title: 'New tab in the active group…', group: 'Layout', icon: 'plus', keys: 'Mod+t', scope: 'global', keywords: 'dock pane plus add open new conversation panel tab here' },
  // The escape hatch: one chord, which cannot be rebound, restores the default layout and keys (with Undo).
  { id: 'layout.reset-all', title: 'Reset the layout and shortcuts', group: 'Layout', icon: 'refresh', keys: 'Ctrl+Alt+Shift+Escape', scope: 'global', fixed: true, overDialogs: true, keywords: 'escape hatch panic restore default keymap keys bindings layout undo stuck' },
  { id: 'snippets.open', title: 'Open snippets', group: 'Conversation', icon: 'snippets', keys: '] ] s', scope: 'global', keywords: 'prompt template' },
  { id: 'composer.focus', title: 'Focus the message box', group: 'Conversation', icon: 'chat', keys: '', scope: 'global', safe: true, keywords: 'type write composer prompt' },
  // Activity detail of the conversation in front (activity-detail.ts).
  { id: 'activity.cycle', title: 'Cycle activity detail', group: 'Conversation', icon: 'activity', keys: '] ] v', scope: 'global', safe: true, keywords: 'quiet smart verbose tool calls commands steps noise collapse expand detail' },
  { id: 'activity.quiet', title: 'Activity detail: Quiet', group: 'Conversation', icon: 'collapse', keys: '', scope: 'global', safe: true, keywords: 'tool calls commands steps hide less noise minimal' },
  { id: 'activity.smart', title: 'Activity detail: Smart', group: 'Conversation', icon: 'sparkle', keys: '', scope: 'global', safe: true, keywords: 'tool calls commands steps summary group collapse' },
  { id: 'activity.verbose', title: 'Activity detail: Verbose', group: 'Conversation', icon: 'expand', keys: '', scope: 'global', safe: true, keywords: 'tool calls commands steps every line all full show' },
  { id: 'activity.default', title: 'Activity detail: Use the default', group: 'Conversation', icon: 'refresh', keys: '', scope: 'global', safe: true, keywords: 'tool calls commands reset settings' },
  // The agent's latest shell code block (code-runs.tsx).
  { id: 'code.run-last', title: 'Run the last command block in the terminal', group: 'Conversation', icon: 'terminal', keys: '] ] r', scope: 'global', keywords: 'shell bash script execute code block terminal prompt login' },
  { id: 'code.run-last-myself', title: 'Run the last command block as command output…', group: 'Conversation', icon: 'output', keys: '', scope: 'global', keywords: 'shell bash script execute code block console yourself myself no terminal' },
  // Lazybox's slash commands (slash-commands.ts): the same commands from the palette.
  { id: 'chat.commands', title: 'Slash commands', group: 'Conversation', icon: 'chevronRight', keys: '', scope: 'global', keywords: 'slash / commands menu agent claude codex' },
  { id: 'chat.model', title: 'Switch model…', group: 'Conversation', icon: 'sparkle', keys: '', scope: 'global', keywords: '/model opus sonnet haiku gpt agent' },
  { id: 'chat.permissions', title: 'Change permissions…', group: 'Conversation', icon: 'shield', keys: '', scope: 'global', keywords: '/permissions ask read only yolo approve' },
  { id: 'chat.subscription', title: 'Switch subscription…', group: 'Conversation', icon: 'card', keys: '', scope: 'global', keywords: '/subscription account login limit' },
  { id: 'chat.browser', title: 'Give the agent the browser', group: 'Conversation', icon: 'globe', keys: '', scope: 'global', keywords: '/browser agent browser chromium web page preview playwright' },
  { id: 'chat.orchestrate', title: 'Let this conversation run agents…', group: 'Conversation', icon: 'subagent', keys: '', scope: 'global', keywords: '/orchestrate orchestrator spawn agents mission grant team' },
  { id: 'chat.compact', title: 'Compact the context', group: 'Conversation', icon: 'collapse', keys: '', scope: 'global', keywords: '/compact summarize tokens shrink' },
  { id: 'chat.clear', title: 'Start over in a new conversation', group: 'Conversation', icon: 'plus', keys: '', scope: 'global', keywords: '/clear reset fresh new keep' },
  { id: 'chat.usage', title: 'Show context and token cost', group: 'Conversation', icon: 'activity', keys: '', scope: 'global', safe: true, keywords: '/cost /context usage tokens window price' },
  { id: 'chat.review', title: 'Open the review tray', group: 'Conversation', icon: 'eye', keys: '', scope: 'global', safe: true, keywords: '/review comments diff changes' },
  { id: 'chat.artifact', title: 'Track the last reply as an artifact…', group: 'Conversation', icon: 'artifact', keys: '', scope: 'global', keywords: '/artifact document report save message' },
  { id: 'chat.stop', title: 'Interrupt the agent', group: 'Conversation', icon: 'stop', keys: '', scope: 'global', keywords: '/stop cancel abort halt' },
  { id: 'chat.rename', title: 'Rename the conversation…', group: 'Conversation', icon: 'pencil', keys: '', scope: 'global', keywords: '/rename title name' },
  { id: 'missions.open', title: 'Show missions', group: 'Workspace', icon: 'target', keys: '', scope: 'global', safe: true, palette: false, keywords: '/mission tasks goals orchestrate' },
  { id: 'missions.page', title: 'Open the missions board', group: 'Workspace', icon: 'target', keys: '', scope: 'global', safe: true, keywords: 'mission board timeline messages leases orchestrator agents plan' },
  { id: 'pulls.open', title: 'Show pull requests', group: 'Workspace', icon: 'pullRequest', keys: '', scope: 'global', safe: true, keywords: 'pull requests prs checks reviews merge github tracked' },
  // Voice (VOICE.md): dictation and spoken commands, held or toggled.
  { id: 'voice.dictate', title: 'Dictate', group: 'Voice', icon: 'mic', keys: 'AltRight or Alt+Space', scope: 'global', hold: true, keywords: 'voice speech speak talk microphone dictation push to talk dictée micro parler' },
  { id: 'voice.command', title: 'Say a command', group: 'Voice', icon: 'mic', keys: 'Alt+Shift+Space', scope: 'global', hold: true, overDialogs: true, keywords: 'voice speech command palette speak talk microphone commande vocale' },
  { id: 'voice.stop-reading', title: 'Stop reading aloud', group: 'Voice', icon: 'stop', keys: '', scope: 'global', keywords: 'voice speech read aloud text to speech silence quiet lecture' },
  { id: 'navigation.next', title: 'Next entry', group: 'Sidebar', icon: 'chevron', keys: 'j', scope: 'navigation', palette: false },
  { id: 'navigation.previous', title: 'Previous entry', group: 'Sidebar', icon: 'up', keys: 'k', scope: 'navigation', palette: false },
  { id: 'navigation.parent', title: 'Workspace / left pane', group: 'Sidebar', icon: 'back', keys: 'h', scope: 'navigation', palette: false },
  { id: 'navigation.open', title: 'Open entry / agent', group: 'Sidebar', icon: 'chevronRight', keys: 'l', scope: 'navigation', palette: false },
  { id: 'chat.send', title: 'Send message', group: 'Message box', icon: 'send', keys: 'Enter', scope: 'agent', palette: false, required: true },
  // While the agent works: the typed message, or else the oldest queued one, goes now; the turn stops for it (QueuedMessages.tsx).
  // While it is idle, a send. Also while dictating: the last words settle first.
  { id: 'chat.send-now', title: 'Send now', group: 'Message box', icon: 'send', keys: 'Mod+Enter', scope: 'agent', keywords: 'interrupt stop queue queued immediately urgent skip the queue' },
  // The help assistant (HelpAssistant.tsx): opens the Ask Lazybox panel and its message box.
  { id: 'help.ask', title: 'Ask Lazybox', group: 'General', icon: 'chat', keys: '] ] h', scope: 'global', keywords: 'help assistant question how docs agent claude codex support', menu: { section: 'Help', title: 'Ask Lazybox…', separatorBefore: true } },
  { id: 'help.welcome', title: 'Welcome and getting started', group: 'General', icon: 'sparkle', keys: '', scope: 'global', overDialogs: true, keywords: 'onboarding first run start tour help', menu: { section: 'Help', title: 'Welcome to Lazybox' } },
  { id: 'help.whats-new', title: "What's new", group: 'General', icon: 'notes', keys: '', scope: 'global', overDialogs: true, keywords: 'release notes changelog update version help', menu: { section: 'Help', title: "What's New" } },
  { id: 'help.docs', title: 'Documentation', group: 'General', icon: 'notes', keys: '', scope: 'global', safe: true, keywords: 'docs manual guide help', menu: { section: 'Help', title: 'Lazybox Documentation' } },
  // Help → Send feedback (FeedbackDialog.tsx): a report the person reads, saves or posts; nothing is uploaded.
  { id: 'help.feedback', title: 'Send feedback', group: 'General', icon: 'chat', keys: '', scope: 'global', keywords: 'feedback bug report issue problem github support diagnostics', menu: { section: 'Help', title: 'Send Feedback…', separatorBefore: true } },
  // Settings → Updates: check now, What's new, install when it suits.
  { id: 'help.updates', title: 'Check for updates', group: 'General', icon: 'download', keys: '', scope: 'global', keywords: 'update upgrade version release install new', menu: { section: 'Help', title: 'Check for Updates…' } },
] as const satisfies readonly ActionDefinition[];

// ⌃1–⌃9 on a Mac (⌘1–⌘9 go to workspaces); Alt+1–Alt+9 elsewhere, where Ctrl+1–Ctrl+9 go to workspaces. 9 is the last tab, as in a browser.
const tabDefinitions = workspaceSlots.map((n): ActionDefinition & { id: `layout.tab.${Slot}` } => ({ id: `layout.tab.${n as Slot}`, title: `Go to tab ${n}`, group: 'Layout', icon: 'panels', keys: `Ctrl+${n}`, otherKeys: `Alt+${n}`, scope: 'global', palette: false, series: 'layout.tab' }));

export type ActionId = (typeof head)[number]['id'] | (typeof tail)[number]['id'] | `workspace.select.${Slot}` | `layout.tab.${Slot}`;
export const actionDefinitions: readonly (ActionDefinition & { id: ActionId })[] = [...head, ...slotDefinitions, ...tail, ...tabDefinitions];
export const actionGroups: readonly ActionGroup[] = ['General', 'Go to', 'Workspace', 'Layout', 'Conversation', 'Voice', 'Sidebar', 'Message box'];
const byId = new Map<string, ActionDefinition>(actionDefinitions.map(definition => [definition.id, definition]));
export function actionDefinition(id: string): ActionDefinition | undefined { return byId.get(id); }

// ---- Keys ----------------------------------------------------------------

export const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
const modifierOrder = ['Ctrl', 'Meta', 'Alt', 'Shift'] as const;
/** The alternatives of one keymap value: "Meta+k or ] ] p" → ["Meta+k", "] ] p"]. */
export function bindings(value: string | undefined): string[] {
  return (value || '').split(/\s+or\s+/).map(binding => binding.trim().replace(/\s+/g, ' ')).filter(Boolean);
}
export function joinBindings(values: readonly string[]): string { return values.join(' or '); }
/** One key with its modifiers in a fixed order, a lone letter lowercased: "Shift+Meta+P" → "Meta+Shift+p". */
export function canonicalKey(key: string): string {
  const parts = key.split('+');
  // "Meta++" names the plus key itself.
  let name = parts.pop() || '';
  if (!name && parts.length) { parts.pop(); name = '+'; }
  const modifiers = new Set(parts.map(part => part === 'Cmd' || part === 'Command' ? 'Meta' : part === 'Control' ? 'Ctrl' : part === 'Option' ? 'Alt' : part));
  if (name.length === 1) name = name.toLowerCase();
  return [...modifierOrder.filter(modifier => modifiers.has(modifier)), name].map((part, i, all) => i === all.length - 1 ? part : part + '+').join('');
}
export function canonicalBinding(binding: string): string { return binding.trim().split(/\s+/).map(canonicalKey).join(' '); }
/** Resolve `Mod+` for a platform: ⌘ on a Mac, Ctrl elsewhere. */
export function resolveMod(value: string, mac = isMac): string {
  return joinBindings(bindings(value).map(binding => canonicalBinding(binding.replaceAll('Mod+', mac ? 'Meta+' : 'Ctrl+'))));
}
/** An action's default keys on a platform, `Mod+` resolved. */
export function defaultKeys(definition: Pick<ActionDefinition, 'keys' | 'otherKeys'>, mac = isMac): string {
  return resolveMod(mac ? definition.keys : definition.otherKeys ?? definition.keys, mac);
}
/**
 * A chord is one key held with Ctrl or ⌘: it fires even from a text field.
 * Off a Mac, Alt with a digit or an arrow is one too: there it types nothing
 * (on a Mac, ⌥ with a digit types a character, so it stays a key you type).
 */
export function isChord(binding: string): boolean { return isChordOn(binding, isMac); }
/** `isChord` for a given platform. */
export function isChordOn(binding: string, mac: boolean): boolean {
  const keys = binding.trim().split(/\s+/);
  if (keys.length !== 1) return false;
  return /(?:^|\+)(?:Ctrl|Meta)\+/.test(keys[0]) || (!mac && /^Alt\+(?:Shift\+)?(?:\d|Arrow(?:Up|Down|Left|Right))$/.test(canonicalKey(keys[0])));
}
/**
 * A modifier on the right of the keyboard, held alone (`AltRight`,
 * `MetaRight`, the KeyboardEvent codes): only a key held to talk can be one.
 * A left one is pressed all the time with other keys.
 */
export const loneModifiers = ['AltRight', 'MetaRight'] as const;
export const isLoneModifier = (binding: string) => (loneModifiers as readonly string[]).includes(binding.trim());
// A no-break space: a binding's keys are joined and split on spaces ("] ] i"), and this is one key.
const loneNames: Record<string, [mac: string, other: string]> = { AltRight: ['Right\u00a0⌥', 'Right\u00a0Alt'], MetaRight: ['Right\u00a0⌘', 'Right\u00a0Win'] };
const symbols: Record<string, string> = { ArrowDown: '↓', ArrowUp: '↑', ArrowLeft: '←', ArrowRight: '→', Enter: '↵', Escape: 'Esc', Backspace: '⌫', Delete: '⌦', Tab: '⇥', Space: 'Space', PageUp: 'Page Up', PageDown: 'Page Down' };
/** How one key of a binding reads on screen: "Meta+Shift+p" → "⇧⌘P" on a Mac, "Ctrl+Shift+P" elsewhere; "AltRight" → "Right ⌥". */
export function displayKey(key: string, mac = isMac): string {
  if (loneNames[key.trim()]) return loneNames[key.trim()][mac ? 0 : 1];
  const canonical = canonicalKey(key), parts = canonical.split('+');
  let name = parts.pop() || ''; if (!name) { parts.pop(); name = '+'; }
  const modifiers = new Set(parts);
  const chord = modifiers.has('Ctrl') || modifiers.has('Meta') || modifiers.has('Alt');
  const shown = symbols[name] || (chord && name.length === 1 ? name.toUpperCase() : name);
  if (mac) return [modifiers.has('Ctrl') ? '⌃' : '', modifiers.has('Alt') ? '⌥' : '', modifiers.has('Shift') ? '⇧' : '', modifiers.has('Meta') ? '⌘' : '', shown].join('');
  return [modifiers.has('Ctrl') ? 'Ctrl+' : '', modifiers.has('Meta') ? 'Win+' : '', modifiers.has('Alt') ? 'Alt+' : '', modifiers.has('Shift') ? 'Shift+' : '', shown].join('');
}
/** Every key of a binding as it reads on screen: "] ] i" → ["]", "]", "i"]. */
export function displayBinding(binding: string, mac = isMac): string[] { return binding.trim().split(/\s+/).filter(Boolean).map(key => displayKey(key, mac)); }

/** The key a chord was pressed as, by physical key, so ⌥ and ⇧ do not change the letter. */
const codeNames: Record<string, string> = { BracketLeft: '[', BracketRight: ']', Slash: '/', Backslash: '\\', Comma: ',', Period: '.', Semicolon: ';', Quote: "'", Backquote: '`', Minus: '-', Equal: '=', Space: 'Space' };
export function chordFromEvent(event: Pick<KeyboardEvent, 'key' | 'code' | 'ctrlKey' | 'metaKey' | 'altKey' | 'shiftKey'>): string {
  const code = event.code || '';
  const name = /^Key[A-Z]$/.test(code) ? code.slice(3).toLowerCase() : /^Digit\d$/.test(code) ? code.slice(5) : codeNames[code] || (event.key.length === 1 ? event.key.toLowerCase() : event.key);
  return `${event.ctrlKey ? 'Ctrl+' : ''}${event.metaKey ? 'Meta+' : ''}${event.altKey ? 'Alt+' : ''}${event.shiftKey ? 'Shift+' : ''}${name}`;
}

// ---- Handlers ------------------------------------------------------------

type Handler = { run(argument?: unknown): void; enabled?(): boolean };
const handlers = new Map<string, Set<{ current: Handler }>>();
let revision = 0;
const listeners = new Set<() => void>();
const changed = () => { revision++; for (const listener of listeners) listener(); };
function subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; }

/** Register handlers for actions while mounted; `false` registers nothing. Latest callbacks are used. */
export function useActions(record: Partial<Record<ActionId, Handler | Handler['run'] | false | undefined>>) {
  const refs = useRef(new Map<string, { current: Handler }>());
  const ids = Object.keys(record).filter(id => record[id as ActionId]).sort().join('|');
  for (const [id, value] of Object.entries(record) as [string, Handler | Handler['run'] | false | undefined][]) {
    if (!value) continue;
    const handler = typeof value === 'function' ? { run: value } : value;
    const ref = refs.current.get(id);
    if (ref) ref.current = handler; else refs.current.set(id, { current: handler });
  }
  useEffect(() => {
    const mine = ids ? ids.split('|') : [];
    for (const id of mine) {
      const ref = refs.current.get(id)!;
      if (!handlers.has(id)) handlers.set(id, new Set());
      handlers.get(id)!.add(ref);
    }
    changed();
    return () => { for (const id of mine) { const ref = refs.current.get(id); if (ref) handlers.get(id)?.delete(ref); } changed(); };
  }, [ids]);
}
/** One handler for one action while mounted. */
export function useAction(id: ActionId, run: (argument?: unknown) => void, enabled?: () => boolean) {
  useActions({ [id]: { run, enabled } } as Partial<Record<ActionId, Handler>>);
}
function enabledHandlers(id: string) {
  return [...handlers.get(id) || []].filter(ref => { try { return ref.current.enabled?.() !== false; } catch { return false; } });
}
/** Whether something in this window can run the action now. */
export function actionEnabled(id: string): boolean { return enabledHandlers(id).length > 0; }
/** Re-renders when handlers come and go. */
export function useActionRevision(): number { return useSyncExternalStore(subscribe, () => revision); }

let lastKeyRun = { id: '', at: 0 };
/**
 * Run an action: every enabled handler in this window, plus listeners of the
 * `lazybox-action` event (the sidebar's j/k navigation). True when a handler ran.
 */
export function runAction(id: string, argument?: unknown, source: 'keys' | 'menu' | 'palette' | 'other' = 'other'): boolean {
  if (source === 'keys') lastKeyRun = { id, at: Date.now() };
  const ready = enabledHandlers(id);
  for (const ref of ready) ref.current.run(argument);
  window.dispatchEvent(new CustomEvent('lazybox-action', { detail: id }));
  return ready.length > 0;
}
/**
 * From the native menu. If the webview already took the key that the menu
 * item names (both saw it), the action ran from the keyboard a moment ago.
 */
export function runMenuAction(id: string): boolean {
  if (lastKeyRun.id === id && Date.now() - lastKeyRun.at < 400) return false;
  return runAction(id, undefined, 'menu');
}

// ---- Views of the registry ----------------------------------------------

export type ResolvedAction = { id: ActionId; title: string; group: ActionGroup; icon: string; keys: string[]; enabled: boolean; run(argument?: unknown): void };
/**
 * Actions by id or group with the current keys and enablement: what a context
 * menu or toolbar shows. `keymap` maps ids to their keys (see keyboard.ts).
 */
export function actionsFor(selector: { ids?: readonly ActionId[]; group?: ActionGroup }, keymap?: Partial<Record<string, string>>): ResolvedAction[] {
  const chosen = selector.ids ? selector.ids.map(id => byId.get(id)).filter((d): d is ActionDefinition => !!d) : actionDefinitions.filter(d => !selector.group || d.group === selector.group);
  return chosen.map(definition => ({
    id: definition.id as ActionId, title: definition.title, group: definition.group, icon: definition.icon,
    keys: bindings(keymap ? keymap[definition.id] : defaultKeys(definition)),
    enabled: actionEnabled(definition.id), run: (argument?: unknown) => { runAction(definition.id, argument); },
  }));
}
/**
 * Context-menu items (ContextMenu's `MenuAction`) for actions by id, with the
 * person's first key as the hint: `registerContextMenu(scope, () => actionMenuItems([...], keymap))`.
 */
export function actionMenuItems(ids: readonly ActionId[], keymap?: Partial<Record<string, string>>): MenuAction[] {
  return actionsFor({ ids }, keymap).map(action => ({ label: action.title, icon: action.icon, shortcut: action.keys[0] ? displayBinding(action.keys[0]).join(' ') : undefined, disabled: !action.enabled, run: () => action.run() }));
}

/** What the native menu bar shows: the menu entries with their first chord as the accelerator. */
export type NativeMenuItem = { id: string; title: string; section: MenuSection; accelerator?: string; separatorBefore: boolean };
export function nativeMenuItems(keymap: Partial<Record<string, string>>): NativeMenuItem[] {
  const accelerator = (binding: string) => binding.split('+').map((part, i, all) => i < all.length - 1 ? ({ Meta: 'Cmd', Ctrl: 'Ctrl', Alt: 'Alt', Shift: 'Shift' } as Record<string, string>)[part] || part : part.length === 1 ? part.toUpperCase() : part).join('+');
  return actionDefinitions.flatMap(definition => {
    const menu = (definition as ActionDefinition).menu;
    if (!menu) return [];
    const chord = bindings(keymap[definition.id]).find(isChord);
    return [{ id: `action:${definition.id}`, title: menu.title, section: menu.section, accelerator: chord ? accelerator(chord) : undefined, separatorBefore: !!menu.separatorBefore }];
  });
}
