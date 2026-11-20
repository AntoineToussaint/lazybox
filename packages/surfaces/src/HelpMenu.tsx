import { ActionMenu } from './ActionMenu';
import { runAction } from './actions';
import { Icon } from './Icons';
import { openOnboarding } from './Onboarding';

/** Help (?) in the window's header: Ask Lazybox, the welcome, the release notes, shortcuts, the docs, feedback and updates. */
export function HelpMenu({ settings = true }: { /** The window has a Settings page (a workspace window does not). */ settings?: boolean }) {
  return <ActionMenu label="Help" className="help-menu" summary={<span className="help-mark" aria-hidden="true">?</span>}>
    <HelpItems settings={settings}/>
  </ActionMenu>;
}
/** Help's items, also listed in the header's ⋯ on a phone, where Help has no button of its own. */
export function HelpItems({ settings = true }: { settings?: boolean }) {
  return <>
    {/* The help assistant panel, its message box focused (HelpAssistant.tsx). */}
    <button onClick={() => runAction('help.ask', undefined, 'menu')}><Icon name="ask" size={14}/><span>Ask Lazybox…</span></button>
    <button onClick={() => runAction('help.training', undefined, 'menu')}><Icon name="keyboard" size={14}/><span>Training Wheels</span></button>
    <button onClick={() => openOnboarding('welcome')}><Icon name="sparkle" size={14}/><span>Welcome &amp; getting started</span></button>
    <button onClick={() => openOnboarding('whats-new')}><Icon name="notes" size={14}/><span>What's new</span></button>
    {settings && <button onClick={() => openOnboarding('shortcuts')}><Icon name="keyboard" size={14}/><span>Keyboard shortcuts</span></button>}
    {/* Bundled with the app and read offline (DocsViewer.tsx). */}
    <button onClick={() => openOnboarding('docs')}><Icon name="notes" size={14}/><span>Documentation</span></button>
    <hr/>
    {/* A report the person reads, then saves, copies or posts as a GitHub issue; nothing is uploaded (FeedbackDialog.tsx). */}
    <button onClick={() => openOnboarding('feedback')}><Icon name="chat" size={14}/><span>Send feedback…</span></button>
    <button onClick={() => openOnboarding('updates')}><Icon name="download" size={14}/><span>Check for updates…</span></button>
  </>;
}
