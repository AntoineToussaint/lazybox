import { useSyncExternalStore } from 'react';
import { bindings, displayBinding, runAction } from './actions';
import { useKeymap } from './keyboard';
import { openDocs } from './Onboarding';
import type { ModuleProps } from './PanelRegistry';
import { coachState, dismissMaximizeTip, enableCoach, subscribeCoach } from './training-wheels';

export function TrainingWheels({ viewState }: ModuleProps) {
  const state = useSyncExternalStore(subscribeCoach, coachState);
  const shortcut = bindings(useKeymap(viewState)['layout.maximize'])[0];
  const tip = state === 'tip' && shortcut && window.matchMedia('(any-pointer: fine)').matches;
  return <section className="training-wheels">
    <label><input type="checkbox" checked={state !== 'off'} onChange={e => enableCoach(e.target.checked)}/> Enable coaching</label>
    {state === 'off' ? <p>Get shortcut tips from the panel controls you use. Counts stay in this window.</p>
      : tip ? <article>
        <h3>Maximize a panel from the keyboard</h3>
        <p>You’ve used the panel controls to maximize or restore the layout three times. Press <kbd>{displayBinding(shortcut).join(' ')}</kbd> to toggle the active panel.</p>
        <button onClick={() => openDocs('reference/keyboard-shortcuts.md')}>Shortcut guide</button>
        <button onClick={() => runAction('help.ask', 'How do I maximize a panel or change its shortcut?')}>Ask Lazybox</button>
        <button onClick={dismissMaximizeTip}>Dismiss tip</button>
      </article> : <p>Tips appear here as you use panel controls.</p>}
  </section>;
}
