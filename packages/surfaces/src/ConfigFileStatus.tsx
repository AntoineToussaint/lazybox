import { useState } from 'react';
import { Icon } from './Icons';
import { errorCount, fileLabel, type ConfigDoc } from './config-files';
import { useConfigFiles } from './useConfigFiles';
import './config-files.css';

const ago = (ms: number) => { const s = Math.max(0, Math.round((Date.now() - ms) / 1000)); return s < 5 ? 'just now' : s < 60 ? `${s}s ago` : s < 3600 ? `${Math.round(s / 60)}m ago` : `${Math.round(s / 3600)}h ago`; };

/** settings.yaml's sections, each shown in its own Settings tab. */
const SETTINGS_SECTIONS = ['notifications', 'appearance'];

/** A path as a person reads it: ~/.config/lazybox/keys.yaml. */
export const homePath = (path: string) => path.replace(/^\/(?:Users|home)\/[^/]+(?=\/)/, '~');

/** The path, with a Copy button that says it copied. */
export function CopyPath({ path, label }: { path: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return <span className="config-path">
    <code title={path}>{homePath(path)}</code>
    <button type="button" className="icon-button" aria-label={`Copy the path of ${label}`} title={copied ? 'Copied' : 'Copy path'} onClick={() => void navigator.clipboard.writeText(path).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1400); }, () => undefined)}>
      <Icon name={copied ? 'check' : 'copy'} size={13}/>
    </button>
  </span>;
}

/** A file's problems, one per line, with the line number first. */
export function ConfigIssues({ doc, appProblem }: { doc: ConfigDoc; appProblem?: string }) {
  if (!doc.issues.length && !appProblem) return null;
  return <ul className="config-issues" aria-label={`Problems in ${fileLabel(doc.name)}`}>
    {appProblem && <li><span className="sev is-error">Error</span><span className="where"/><span>{appProblem}</span></li>}
    {doc.issues.map((issue, i) => <li key={i}>
      <span className={`sev is-${issue.severity}`}>{issue.severity === 'error' ? 'Error' : 'Warning'}</span>
      <span className="where">{issue.line ? `line ${issue.line}` : ''}</span>
      <span title={issue.path}>{issue.message}</span>
    </li>)}
  </ul>;
}

/**
 * Where a Settings section is kept as a file (POWER.md, "Config files"): its
 * path, whether it exists, and what is wrong with it by line. A file that does
 * not load says so; the last version that loaded stays in force.
 */
export function ConfigFileStatus({ name, what, section }: { name: string; what: string; /** Only this part of the file's problems ("appearance"): a file several Settings sections share. */ section?: string }) {
  const files = useConfigFiles();
  const whole = files.docs.find(d => d.name === name);
  // A problem of another section is that section's; one of the file as a whole shows everywhere.
  const doc = whole && section ? { ...whole, issues: whole.issues.filter(i => { const top = i.path.split(/[.[]/)[0]; return top === section || !SETTINGS_SECTIONS.includes(top); }) } : whole;
  if (!files.loaded || !doc) return files.error ? <p className="config-file-line is-muted" role="status"><Icon name="file" size={14}/><span>{what} are kept in this app; their file could not be read ({files.error})</span></p> : null;
  const errors = errorCount(doc), appProblem = files.appProblems[name];
  const state = !doc.exists ? 'Not created yet: your first change here creates it' : doc.error ? 'Does not load' : `Loaded ${ago(doc.loadedAt)}${errors ? ` · ${errors === 1 ? '1 problem' : `${errors} problems`}` : ''}`;
  return <div className={`config-file${errors || appProblem ? ' has-problems' : ''}`}>
    <div className="config-file-line">
      <Icon name="file" size={14}/>
      <span className="config-file-text"><span>{what} are kept in <strong>{fileLabel(name)}</strong>; edit it in any editor and Lazybox applies it as you save.</span><small>{state}</small></span>
      <CopyPath path={doc.path} label={fileLabel(name)}/>
    </div>
    {doc.error && <p className="config-file-banner" role="alert"><Icon name="alert" size={14}/><span><strong>{fileLabel(name)} does not load:</strong> {doc.error.replace(/[^.]$/, '$&.')}{doc.stale ? ' Lazybox keeps using the last version that loaded until you fix it.' : ''}</span></p>}
    <ConfigIssues doc={doc} appProblem={appProblem}/>
  </div>;
}
