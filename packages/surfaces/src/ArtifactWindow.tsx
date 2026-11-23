import { useEffect, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { Collection } from '@lazybox/protocol';
import { AppearanceSync } from './appearance/AppearanceSync';
import { ArtifactCompanion } from './ArtifactCompanion';
import { AttentionScope } from './useAttention';
import { RunnerProvider, useRunner, type RunnerConnection } from './RunnerContext';
import './artifacts.css';

type ArtifactView = { collectionId: string; artifactId: string; viewState?: ViewStateStore };

/** A standalone artifact can share the window with a chat from its workspace. */
export function ArtifactWindow({ connection, ...view }: ArtifactView & { connection: RunnerConnection }) {
  return <RunnerProvider connection={connection}><AppearanceSync store={view.viewState}/><div className="artifact-window workspace-shell"><AttentionScope root={view.viewState?.root}><StandalonePage key={view.artifactId} {...view}/></AttentionScope></div></RunnerProvider>;
}

function StandalonePage({ collectionId, artifactId, viewState }: ArtifactView) {
  const { api } = useRunner();
  const [collection, setCollection] = useState<Collection>();
  useEffect(() => {
    const control = new AbortController();
    void api.getCollection({ collectionId }, { signal: control.signal, timeoutMs: 10000 }).then(result => { if (!control.signal.aborted) setCollection(result.collection); }, () => {});
    return () => control.abort();
  }, [api, collectionId]);
  useEffect(() => { document.title = collection ? `Artifact · ${collection.name}` : 'Artifact'; }, [collection]);
  if (!collectionId || !artifactId) return <p className="module-empty">This window has no artifact to show.</p>;
  return <ArtifactCompanion collectionId={collectionId} artifactId={artifactId} collection={collection}
    viewState={viewState} standalone onDeleted={() => window.close()}/>;
}
