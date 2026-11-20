import { useDisplayDetail } from './display-detail';
import { useMemo } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { ACTIVITY_LABELS, useActivityDefault } from './activity-detail';
import { ChatTextSettings } from './AppearanceSettings';
import { SettingsGroup, SettingsHeader } from './settings-parts';
import { ToolLineContext, ToolLines } from './ToolLine';
import { ACTIVITY_DETAILS, type ToolEntry } from './tool-lines';
import { useTabsSetting } from './useConversationTabs';

const ROOT = '/work/api';
const at = (s: number) => 1_700_000_000_000 + s * 1000;
/** A short, typical run of an agent's work, to show what each choice looks like. */
const SAMPLE: ToolEntry[] = [
  { id: 's1', name: 'Bash', status: 'completed', input: { command: `cd ${ROOT} && sed -n '1,120p' internal/runtime/runtime_access.go`, description: 'Read the access check' }, output: '…', exitCode: 0, startedAt: at(0), endedAt: at(0.2) },
  { id: 's2', name: 'Bash', status: 'completed', input: { command: `rg -n "resolveAccess" ${ROOT}/internal` }, output: '…', exitCode: 0, startedAt: at(1), endedAt: at(1.3) },
  { id: 's3', name: 'Read', status: 'completed', input: { file_path: `${ROOT}/internal/runtime/policy.go` }, output: 'a\nb\n', outputLines: 84, startedAt: at(2), endedAt: at(2.1) },
  { id: 's4', name: 'Thinking', status: 'completed', startedAt: at(3), endedAt: at(15) },
  { id: 's5', name: 'Bash', status: 'failed', input: { command: 'go test ./internal/runtime/...', description: 'Run the runtime tests' }, output: 'Exit code 1\n--- FAIL: TestResolveAccess (0.01s)', exitCode: 1, startedAt: at(16), endedAt: at(24) },
  { id: 's6', name: 'Edit', status: 'completed', input: { file_path: `${ROOT}/internal/runtime/runtime_access.go`, old_string: 'return false', new_string: 'return owner == caller' }, diff: { added: 1, removed: 1, files: [`${ROOT}/internal/runtime/runtime_access.go`] }, patch: '-return false\n+return owner == caller\n', startedAt: at(25), endedAt: at(25.1) },
  { id: 's7', name: 'Bash', status: 'completed', input: { command: 'go test ./internal/runtime/...', description: 'Run the runtime tests again' }, output: 'ok', exitCode: 0, startedAt: at(26), endedAt: at(33) },
];

/** Settings → Chat: how much of the agents' tool activity conversations show. */
export function ChatSettings({ store }: { store?: ViewStateStore }) {
  const display = useDisplayDetail(store);
  const [detail, setDetail] = useActivityDefault(store);
  const [tabs, setTabs] = useTabsSetting(store);
  const scope = useMemo(() => ({ root: ROOT, detail }), [detail]);
  return <section className="chat-settings" aria-labelledby="chat-settings-title">
    <SettingsHeader id="chat-settings-title" title="Chat" description="How conversations read and how much of the agents’ work they show."/>
    <SettingsGroup id="chat-conversation-tabs" title="Tabs" description="Conversations open as tabs of the chat, like files in an editor.">
      <label className="settings-row">
        <span className="settings-row-text"><span className="settings-row-label">Open a workspace’s conversations as tabs</span>
          <small>{tabs ? 'Each conversation you open stays in a tab, and a workspace opens with its conversations as tabs (the six most recent).' : 'A single click shows a conversation in the preview tab, in italics, which the next one replaces; double-click to keep it.'}</small></span>
        <span className="settings-row-control settings-switch"><input type="checkbox" role="switch" aria-label="Open a workspace’s conversations as tabs" checked={tabs} onChange={event => setTabs(event.target.checked)}/></span>
      </label>
    </SettingsGroup>
    <SettingsGroup id="chat-activity-detail" title="Activity detail" description={<>How much of an agent’s tool calls a conversation shows. Change one conversation from its ⋯ menu, the command palette, or <kbd>]</kbd> <kbd>]</kbd> <kbd>v</kbd>. Every call stays in the Activity dialog.</>}>
      <div role="radiogroup" aria-label="Activity detail">
        {ACTIVITY_DETAILS.map(choice => <label key={choice} className="settings-row chat-settings-choice">
          <span className="settings-row-text"><span className="settings-row-label">{ACTIVITY_LABELS[choice].label}{choice === 'smart' ? ' · recommended' : ''}</span><small>{ACTIVITY_LABELS[choice].detail}</small></span>
          <span className="settings-row-control"><input type="radio" name="activity-detail" value={choice} checked={detail === choice} onChange={() => setDetail(choice)}/></span>
        </label>)}
      </div>
    </SettingsGroup>
    <SettingsGroup title="Activity preview" description="The same work, as a conversation shows it now. Click a line to open it.">
      <div className="chat-settings-preview" aria-label={`Preview of ${ACTIVITY_LABELS[detail].label}`}>
        <p>I’ll check how the runtime resolves access.</p>
        <ToolLineContext.Provider value={scope}><ToolLines entries={SAMPLE} turn={SAMPLE}/></ToolLineContext.Provider>
        <p>Fixed: the owner can now read their own runtime, and the tests pass.</p>
      </div>
    </SettingsGroup>
    <SettingsGroup title="Timestamps" description="Reply times are available on hover, with keyboard focus, or by tapping the clock on touch screens.">
      <label className="settings-row"><span className="settings-row-label">Always show timestamps</span>
        <input type="checkbox" role="switch" checked={display.timestamps} onChange={event => display.customize({ timestamps: event.target.checked })}/>
      </label>
    </SettingsGroup>
    <ChatTextSettings store={store}/>
  </section>;
}
