import { useEffect, useRef } from 'react';
import type { Panel } from './workspace-model';

/**
 * One element per displayed panel, created once: React renders each panel into its own
 * host (a portal) and the layout only moves that host, so docking or closing a panel
 * never remounts the others. Hosts are keyed by kind and occurrence ("chat:0"), not by
 * panel ID, so the same panel also stays mounted when another workspace's page (with
 * its own IDs) takes its place, as the workspace list must.
 */
export function usePanelHosts(shownPanels: Panel[]) {
  const hosts = useRef(new Map<string, HTMLDivElement>());
  const host = (slot: string) => {
    let element = hosts.current.get(slot);
    if (!element) { element = document.createElement('div'); element.className = 'panel-host'; hosts.current.set(slot, element); }
    return element;
  };
  const slots = new Map<string, string>(), occurrences = new Map<string, number>();
  for (const panel of shownPanels) { const n = occurrences.get(panel.kind) || 0; occurrences.set(panel.kind, n + 1); slots.set(panel.id, `${panel.kind}:${n}`); }
  useEffect(() => {
    const shown = new Set(slots.values());
    for (const slot of hosts.current.keys()) if (!shown.has(slot)) hosts.current.delete(slot);
  });
  return {
    /** The portal key of a displayed panel. */
    slotOf: (panel: Panel) => slots.get(panel.id) || panel.id,
    hostFor: (panel: Panel) => host(slots.get(panel.id) || panel.id),
  };
}
