import { useState, type RefObject } from 'react';
import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import { useContextMenuContribution, type MenuItem } from './ContextMenu';
import { artifactsChanged, errorText } from './artifact-client';
import { openArtifact } from './artifact-events';
import { trackableFile } from './artifact-model';
import type { FileLink } from './file-reference';
import { PathChooser, type PathChoice } from './file-links';
import { useRunner } from './RunnerContext';
import type { Workbench } from './workbench';

export { trackableFile } from './artifact-model';

/** What tracking a file said: nothing when its artifact opened, a note when it could not be shown, or why it failed. */
export type TrackOutcome = { text: string; failed: boolean };

/**
 * Track a workspace file as an artifact and show it. Tracking a file that is
 * already tracked opens that artifact; the Runner records a version whenever
 * the file changes on disk.
 */
export async function trackFileOutcome(api: RunnerClient, collectionId: string, path: string): Promise<TrackOutcome> {
  try {
    const artifact = await api.trackArtifactFile({ collectionId, path }, { timeoutMs: 15000 });
    artifactsChanged(collectionId);
    return { text: openArtifact({ collectionId, artifactId: artifact.id }) ? '' : `Tracking “${artifact.title}”. Open it from the Artifacts panel.`, failed: false };
  } catch (error) { return { text: `Could not track ${path}: ${errorText(error)}`, failed: true }; }
}
export const trackFile = async (api: RunnerClient, collectionId: string, path: string) => (await trackFileOutcome(api, collectionId, path)).text;

/**
 * The workspace files a file link names, resolved the way opening it is
 * (`useFileOpener`): workspace-relative paths, folders left out. An older
 * Runner that cannot resolve gets the path as written.
 */
export async function linkedWorkspaceFiles(api: RunnerClient, collectionId: string, path: string): Promise<{ paths: string[]; truncated: boolean }> {
  try {
    const resolved = await api.resolveWorkspacePath({ collectionId, path }, { timeoutMs: 10000 });
    return { paths: resolved.matches.filter(match => !match.directory).map(match => match.path), truncated: resolved.truncated };
  } catch (error) {
    if (ConnectError.from(error).code === Code.Unimplemented) return { paths: [path], truncated: false };
    throw error;
  }
}

const trackItem = (path: string, run: () => void): MenuItem => {
  const markdown = trackableFile(path);
  return { label: 'Track as artifact', icon: 'artifact', disabled: !markdown && 'Markdown files only', note: markdown ? undefined : 'Markdown only', run };
};

/**
 * Tracking from the Files tree: "Track as artifact" in a file row's context
 * menu (the `file` scope of the app's context menus, which also opens, quick
 * looks and copies paths), and the notice saying where the artifact went.
 */
export function useFileContextMenu(workbench: Workbench | undefined) {
  const [notice, onNotice] = useState('');
  useContextMenuContribution<{ path: string; directory: boolean }>('file', ({ detail }) => {
    if (!workbench || detail.directory) return [];
    return ['separator', trackItem(detail.path, () => void trackFile(workbench.api, workbench.workspaceId, detail.path).then(text => { if (text) onNotice(text); }))];
  });
  return { element: <>{notice && <p className="tree-status file-artifact-notice" role="status">{notice}<button type="button" className="link-button" onClick={() => onNotice('')}>Dismiss</button></p>}</> };
}

/**
 * Tracking from a file an agent named: "Track as artifact" in a file link's
 * context menu (the `file-link` scope), for the links inside `container` (a
 * conversation of workspace `collectionId`). The link is resolved like Open
 * resolves it; several matches become a choice, and a path the workspace
 * does not hold goes to the Runner as written, so its refusal is what shows.
 */
export function useFileLinkTracking(collectionId: string | undefined, container: RefObject<Element | null>) {
  const { api } = useRunner();
  const [notice, setNotice] = useState<TrackOutcome>(), [choice, setChoice] = useState<PathChoice>();
  const track = async (path: string) => { const outcome = await trackFileOutcome(api, collectionId!, path); setNotice(outcome.text ? outcome : undefined); };
  const trackLink = async (link: FileLink) => {
    setNotice(undefined);
    try {
      const { paths, truncated } = await linkedWorkspaceFiles(api, collectionId!, link.path);
      if (paths.length > 1) setChoice({ reference: link, matches: paths.map(path => ({ path, directory: false })), truncated, how: {} });
      else await track(paths[0] || link.path);
    } catch (error) { setNotice({ text: `Could not track ${link.path}: ${errorText(error)}`, failed: true }); }
  };
  useContextMenuContribution<FileLink>('file-link', ({ element, detail }) => {
    if (!collectionId || !container.current?.contains(element)) return [];
    return ['separator', trackItem(detail.path, () => void trackLink(detail))];
  });
  return {
    element: <>
      {choice && <PathChooser choice={choice} onChoose={match => { setChoice(undefined); void track(match.path); }} onClose={() => setChoice(undefined)}/>}
      {notice && <div className={'file-link-notice' + (notice.failed ? '' : ' is-quiet')} role={notice.failed ? 'alert' : 'status'}><span>{notice.text}</span><button className="icon-button" aria-label="Dismiss" onClick={() => setNotice(undefined)}>×</button></div>}
    </>,
  };
}
