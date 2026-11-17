import { ConnectError, type RunnerClient } from '@lazybox/client-core';

/** The Runner's own sentence for a failed call, without the transport prefix. */
export const errorText = (error: unknown) => ConnectError.from(error).rawMessage || String(error);

const CHANGED = 'lazybox-artifacts-changed';
/** Tell every artifact list of this workspace in this window to reload now (after a write). */
export const artifactsChanged = (collectionId: string) => window.dispatchEvent(new CustomEvent(CHANGED, { detail: collectionId }));
export const onArtifactsChanged = (listener: (collectionId: string) => void) => {
  const handler = (event: Event) => listener((event as CustomEvent<string>).detail);
  window.addEventListener(CHANGED, handler);
  return () => window.removeEventListener(CHANGED, handler);
};

/**
 * Retire an artifact (kept with every version, out of the list) or restore
 * retired ones. Nothing here deletes; every list of the workspace reloads.
 */
export async function retireArtifact(api: RunnerClient, collectionId: string, artifactId: string) {
  const artifact = await api.retireArtifact({ collectionId, artifactId }, { timeoutMs: 10000 });
  artifactsChanged(collectionId);
  return artifact;
}
export async function restoreArtifacts(api: RunnerClient, collectionId: string, artifactIds: readonly string[]) {
  try { for (const artifactId of artifactIds) await api.retireArtifact({ collectionId, artifactId, restore: true }, { timeoutMs: 10000 }); }
  finally { artifactsChanged(collectionId); }
}
