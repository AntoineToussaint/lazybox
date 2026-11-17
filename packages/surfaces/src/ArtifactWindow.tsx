import { AppearanceSync } from './appearance/AppearanceSync';
import { useEffect, useState } from 'react';
import type { Collection } from '@lazybox/protocol';
import { ArtifactPage } from './ArtifactPage';
import { RunnerProvider, useRunner, type RunnerConnection } from './RunnerContext';
import './artifacts.css';

/**
 * An artifact in its own window (browser or native): the same page as in the
 * Artifacts panel, following new versions on its own. Closing it changes nothing.
 */

export function ArtifactWindow({ connection, collectionId, artifactId }: { connection: RunnerConnection; collectionId: string; artifactId: string }) {
  return <RunnerProvider connection={connection}><AppearanceSync/><div className="artifact-window workspace-shell"><StandalonePage collectionId={collectionId} artifactId={artifactId}/></div></RunnerProvider>;
}

function StandalonePage({ collectionId, artifactId }: { collectionId: string; artifactId: string }) {
  const { api } = useRunner();
  const [collection, setCollection] = useState<Collection>();
  useEffect(() => {
    const control = new AbortController();
    void api.getCollection({ collectionId }, { signal: control.signal, timeoutMs: 10000 }).then(result => { if (!control.signal.aborted) setCollection(result.collection); }, () => {});
    return () => control.abort();
  }, [api, collectionId]);
  useEffect(() => { document.title = collection ? `Artifact · ${collection.name}` : 'Artifact'; }, [collection]);
  if (!collectionId || !artifactId) return <p className="module-empty">This window has no artifact to show.</p>;
  return <ArtifactPage collectionId={collectionId} artifactId={artifactId} collection={collection} standalone onDeleted={() => window.close()}/>;
}
