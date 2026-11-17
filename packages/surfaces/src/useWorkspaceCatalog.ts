import { useEffect, useState } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { Collection } from '@lazybox/protocol';

/** The Runner's workspaces, polled while the window is open. */
export function useWorkspaceCatalog(api: RunnerClient) {
  const [collections, setCollections] = useState<Collection[]>([]), [catalogError, setCatalogError] = useState(''), [revision, setRevision] = useState(0);
  // Whether this window has heard the Runner's list at all: until then there is
  // nothing to show, which is not the same as having no workspaces.
  const [catalogLoaded, setCatalogLoaded] = useState(false);
  useEffect(() => {
    const control = new AbortController(); let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      // Bounded: a request stuck on a Runner that is going away must not stop
      // this poll from reaching its successor.
      try { const result = await api.listCollections({}, { signal: control.signal, timeoutMs: 10000 }); if (!control.signal.aborted) { setCollections(result.collections); setCatalogLoaded(true); setCatalogError(''); } }
      catch (error) { if (!control.signal.aborted) setCatalogError(String(error)); }
      finally { updating = false; }
    };
    void update(); const timer = setInterval(() => void update(), 3000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, revision]);
  const refreshCatalog = () => setRevision(value => value + 1);
  return { collections, catalogError, catalogLoaded, refreshCatalog };
}
