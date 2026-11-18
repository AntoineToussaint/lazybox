import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import type { AddComputerStep, RunnerDirectoryHost, SshHost } from '@lazybox/platform';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { showToast } from './toasts';

/**
 * Settings → Runners → Add a computer (REMOTE_RUNNERS.md §7): the hosts of
 * the person's ~/.ssh/config in one filtered list (or any `user@host` typed),
 * then the steps of `lazybox remote add` as they happen, each with a tick or
 * the exact error. Closing the sheet does not stop a step that is running:
 * the computer still gets added, and a notice says when.
 */
export function AddComputerSheet({ host, onClose, onAdded }: { host: RunnerDirectoryHost; onClose(): void; onAdded(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [hosts, setHosts] = useState<SshHost[] | null>(null);
  const [hostsError, setHostsError] = useState('');
  const [filter, setFilter] = useState('');
  const [selected, setSelected] = useState(0);
  const [chosen, setChosen] = useState('');
  const [steps, setSteps] = useState<AddComputerStep[]>([]);
  const [error, setError] = useState('');
  const [done, setDone] = useState(false);
  const mounted = useRef(true), field = useRef<HTMLInputElement>(null);
  // The filter has the focus as the sheet opens: type a host, arrows, Return.
  useEffect(() => { mounted.current = true; dialog.current?.showModal(); field.current?.focus(); return () => { mounted.current = false; }; }, []);
  useEffect(() => {
    let live = true;
    host.sshHosts?.().then(list => { if (live) setHosts(list); }, failure => { if (live) { setHosts([]); setHostsError(message(failure)); } });
    return () => { live = false; };
  }, [host]);
  const matches = useMemo(() => {
    const query = filter.trim().toLowerCase();
    const list = (hosts || []).filter(h => !query || [h.name, h.hostName, h.user].some(v => v?.toLowerCase().includes(query)));
    // Anything typed that is not in the list can still be an ssh destination.
    if (query && /^[A-Za-z0-9_[][A-Za-z0-9_.@%:[\]+-]*$/.test(filter.trim()) && !list.some(h => h.name.toLowerCase() === query)) list.push({ name: filter.trim(), hostName: '', source: 'typed' });
    return list;
  }, [hosts, filter]);
  useEffect(() => { setSelected(index => Math.min(index, Math.max(matches.length - 1, 0))); }, [matches.length]);
  const running = !!chosen && !done && !error;

  const add = (name: string) => {
    if (!host.addComputer || chosen) return;
    setChosen(name); setSteps([]); setError(''); setDone(false);
    const step = (next: AddComputerStep) => { if (mounted.current) setSteps(previous => [...previous.filter(s => s.step !== next.step), next]); };
    host.addComputer(name, step).then(() => {
      onAdded();
      if (mounted.current) setDone(true);
      else showToast({ id: `computer:${name}`, tone: 'info', title: `${name} is added`, body: 'Its Runner shows in the sidebar.' });
    }, failure => {
      if (mounted.current) setError(message(failure));
      else showToast({ id: `computer:${name}`, tone: 'failed', title: `Could not add ${name}`, body: message(failure) });
    });
  };
  const keys = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const step = event.key === 'ArrowDown' ? 1 : -1;
      setSelected(index => matches.length ? (index + step + matches.length) % matches.length : 0);
    } else if (event.key === 'Enter') {
      event.preventDefault();
      const target = matches[selected];
      if (target) add(target.name);
    }
  };
  const close = () => {
    if (running) showToast({ id: `computer:${chosen}`, tone: 'info', title: `Adding ${chosen}`, body: 'It goes on in the background; a notice says when it is done.' });
    onClose();
  };

  return <DraggableDialog ref={dialog} className="add-computer" aria-label="Add a computer" draft="kept" onCancel={event => { event.preventDefault(); close(); }}>
    <DialogHeader><h2><Icon name="cloud" size={18}/>{chosen ? `Add ${chosen}` : 'Add a computer'}</h2><button type="button" aria-label="Close" onClick={close}><Icon name="close"/></button></DialogHeader>
    <div className="add-computer-body">
      {!chosen && <>
        <p className="add-computer-note">A computer you can <code>ssh</code> into: a Linux box, a cloud VM, another Mac. Lazybox installs its Runner there if needed, starts it, and adds this computer as its owner. Agents there keep running when this computer sleeps.</p>
        <input ref={field} className="add-computer-filter" data-autofocus aria-label="Filter hosts, or type user@host" placeholder="Filter your SSH hosts, or type user@host" spellCheck={false} autoComplete="off"
          role="combobox" aria-expanded aria-controls="add-computer-hosts" aria-activedescendant={matches[selected] ? `add-computer-host-${selected}` : undefined}
          value={filter} onChange={event => { setFilter(event.target.value); setSelected(0); }} onKeyDown={keys}/>
        {hosts === null ? <p className="add-computer-note" role="status">Reading ~/.ssh/config…</p>
          : <ul id="add-computer-hosts" className="add-computer-hosts" role="listbox" aria-label="SSH hosts">
            {matches.map((h, index) => <li key={h.name} role="presentation">
              <button type="button" id={`add-computer-host-${index}`} role="option" aria-selected={index === selected} tabIndex={-1} disabled={!!h.added}
                onMouseEnter={() => setSelected(index)} onClick={() => add(h.name)} title={h.added ? `Added as ${h.added}` : `Add ${h.name}`}>
                <Icon name={h.source === 'typed' ? 'plus' : 'cloud'} size={13}/>
                <span>{h.source === 'typed' ? <>Add <strong>{h.name}</strong></> : h.name}</span>
                <small>{h.added ? 'Added' : [h.user, h.hostName].filter(Boolean).join('@')}</small>
              </button>
            </li>)}
            {!matches.length && <li className="add-computer-note" role="presentation">{hostsError || 'No Host in ~/.ssh/config matches. Type user@host to use another.'}</li>}
          </ul>}
      </>}
      {chosen && <>
        <ol className="add-computer-steps" aria-label="Steps" aria-live="polite">
          {steps.map(step => <li key={step.step} data-state={step.state}>
            <Icon name={step.state === 'done' ? 'check' : step.state === 'failed' ? 'alert' : step.state === 'skipped' ? 'skip' : 'pending'} size={14}/>
            <span>{step.title}</span>
            {step.detail && <small>{step.detail}</small>}
          </li>)}
          {!steps.length && !error && <li data-state="running"><Icon name="pending" size={14}/><span>Connect to {chosen}</span></li>}
        </ol>
        {error && <p className="add-computer-error" role="alert">{error}</p>}
        {done && <p className="add-computer-note" role="status">{chosen} is added. Its Runner shows in the sidebar, under its own name; open its workspaces as on this computer.</p>}
        <div className="add-computer-actions">
          {error && <button type="button" onClick={() => { setChosen(''); setError(''); setSteps([]); }}>Back</button>}
          <button type="button" className={done ? 'primary' : ''} onClick={close}>{running ? 'Close (keeps going)' : done ? 'Done' : 'Close'}</button>
        </div>
      </>}
    </div>
  </DraggableDialog>;
}

const message = (failure: unknown) => String(failure instanceof Error ? failure.message : failure).replace(/^Error: /, '');
