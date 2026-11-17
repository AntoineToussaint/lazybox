import { useEffect, useMemo, useRef, useState } from 'react';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { cycleLayouts, layoutFileName, layoutNameOf, layoutsFromFiles, layoutYaml, type ConfigDoc, type FileLayout } from './config-files';
import { sameTemplate, type LayoutPreset, type TemplateNode } from './layout-presets';
import { configDocs, updateConfigFile, useConfigFiles } from './useConfigFiles';
import { showToast } from './toasts';
import './config-files.css';

/**
 * The person's layouts as files (POWER.md, "Config files"): layouts/*.yaml
 * beside the actions file, listed after the presets. Saving, renaming and
 * deleting write the files (deleting offers Undo); a file dropped into the
 * folder shows within two seconds; one with a mistake is listed with its line.
 */
export function useLayoutFiles() {
  const files = useConfigFiles();
  const layouts = useMemo(() => layoutsFromFiles(files.docs), [files.docs]);
  const broken = useMemo(() => files.docs.filter(d => d.kind === 'layout' && (!d.value || d.error)), [files.docs]);
  const names = () => files.docs.map(d => d.name);
  const fail = (what: string) => (error: unknown) => { showToast({ id: 'layout-file', tone: 'failed', title: `Could not ${what}`, body: error instanceof Error ? error.message : String(error) }); throw error; };
  return {
    layouts, broken, folder: files.directory ? `${files.directory}/layouts` : '',
    /** Saves the arrangement as a layout file; a layout of the same name is replaced. */
    save: async (name: string, layout: TemplateNode) => {
      const same = layouts.find(l => l.name === name);
      const file = same?.file || layoutFileName(name, names());
      await updateConfigFile({ name: file, content: layoutYaml({ name, layout }) }).catch(fail('save the layout'));
      showToast({ id: 'layout-file', tone: 'info', title: `Saved “${name}”`, body: `${file}.yaml in your Lazybox folder`, timeout: 3200 });
    },
    rename: (id: string, name: string) => {
      const layout = layouts.find(l => l.id === id);
      if (layout) void updateConfigFile({ name: layout.file, edits: [{ path: ['name'], json: JSON.stringify(name) }] }).catch(fail('rename the layout')).catch(() => undefined);
    },
    /** Deletes a layout's file, with Undo (which writes it back). */
    remove: (id: string) => {
      const layout = layouts.find(l => l.id === id);
      if (!layout) return;
      const content = layoutYaml(layout);
      void updateConfigFile({ name: layout.file, remove: true }).then(() => showToast({ id: 'layout-file', tone: 'info', title: `Deleted “${layout.name}”`, body: `${layout.file}.yaml`, timeout: 8000,
        button: { label: 'Undo', run: () => void updateConfigFile({ name: layout.file, content }).catch(fail('put the layout back')).catch(() => undefined) } }), fail('delete the layout')).catch(() => undefined);
    },
    /** Puts a layout's YAML on the clipboard. */
    copy: (layout: { name: string; description?: string; layout: TemplateNode }) => {
      void navigator.clipboard.writeText(layoutYaml(layout)).then(
        () => showToast({ id: 'layout-file', tone: 'info', title: `Copied “${layout.name}” as YAML`, body: 'Paste it into ⌘K on another machine, or save it in layouts/.', timeout: 3200 }),
        () => showToast({ id: 'layout-file', tone: 'failed', title: 'Could not copy to the clipboard' }));
    },
    /** Every layout the cycle key steps through, and the one after the arrangement shown. */
    next: (current: TemplateNode | null): { layout: LayoutPreset; index: number; count: number } => {
      const all = cycleLayouts(layouts);
      const at = all.findIndex(l => sameTemplate(current, l.layout));
      const index = (at + 1) % all.length;
      return { layout: all[index], index, count: all.length };
    },
    names,
  };
}

/** The issues of a refusal the Runner sent ("…: error: line 5 (path): message; …"), each with its line. */
function refusalLines(message: string): { line: string; message: string }[] {
  const at = message.indexOf('error: ');
  if (at < 0) return [{ line: '', message }];
  return message.slice(at).replace(/\.$/, '').split(/;\s+(?=(?:error|warning): )/).map(text => {
    const match = /^(?:error|warning): (?:line (\d+) \([^)]*\): )?(.*)$/s.exec(text);
    return { line: match?.[1] ? `line ${match[1]}` : '', message: match?.[2] || text };
  });
}

/**
 * Import a layout from its YAML: pasted here, or from ⌘K when the clipboard
 * holds one. The Runner checks it first; a mistake is listed by line and
 * nothing is written. An imported layout is applied at once.
 */
export function LayoutImportDialog({ initial, onClose, onImported }: { initial: string; onClose(): void; onImported(layout: FileLayout): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [text, setText] = useState(initial);
  const [problems, setProblems] = useState<{ line: string; message: string }[]>([]);
  const [busy, setBusy] = useState(false);
  const files = useConfigFiles();
  useEffect(() => { dialog.current?.showModal(); requestAnimationFrame(() => dialog.current?.querySelector<HTMLTextAreaElement>('textarea')?.focus()); }, []);
  // Nothing passed in: offer what the clipboard holds, when it reads as a layout.
  useEffect(() => {
    if (initial) return;
    void navigator.clipboard?.readText?.().then(value => { if (/(^|\n)\s*layout\s*:/.test(value)) setText(current => current || value); }, () => undefined);
  }, [initial]);
  const submit = async () => {
    setBusy(true); setProblems([]);
    const name = layoutFileName(layoutNameOf(text), files.docs.map((d: ConfigDoc) => d.name));
    try {
      await updateConfigFile({ name, content: text.endsWith('\n') ? text : `${text}\n` });
      const layout = layoutsFromFiles(configDocs()).find(l => l.file === name);
      if (layout) onImported(layout);
      onClose();
    } catch (error) { setProblems(refusalLines(error instanceof Error ? error.message : String(error))); }
    finally { setBusy(false); }
  };
  return <DraggableDialog ref={dialog} className="layout-import" aria-labelledby="layout-import-title" onCancel={event => { event.preventDefault(); onClose(); }}>
    <DialogHeader><h2 id="layout-import-title">Import a layout</h2><button aria-label="Close" onClick={onClose}>×</button></DialogHeader>
    <p>Paste a layout’s YAML, as “Copy the layout as YAML” writes it. It is saved in your layouts folder and applied.</p>
    <textarea aria-label="Layout YAML" spellCheck={false} wrap="off" value={text} placeholder={'name: Review wide\nlayout:\n  split: row\n  sizes: [20, 40, 40]\n  children:\n    - panels: [collections]\n    - panels: [changes]\n    - panels: [chat]'}
      onChange={event => { setText(event.target.value); setProblems([]); }}
      onKeyDown={event => { if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && text.trim()) { event.preventDefault(); void submit(); } }}/>
    {problems.length > 0 && <ul className="config-issues" role="alert" aria-label="Problems in the layout">{problems.map((problem, i) => <li key={i}><span className="sev is-error">Error</span><span className="where">{problem.line}</span><span>{problem.message}</span></li>)}</ul>}
    <footer><small>⌘↵ to import · Esc to close</small><button type="button" onClick={onClose}>Cancel</button><button type="button" className="primary" disabled={!text.trim() || busy} onClick={() => void submit()}><Icon name="download" size={13}/> Import</button></footer>
  </DraggableDialog>;
}

