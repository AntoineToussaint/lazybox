import { createContext } from 'react';

/** Provided by the Ask Lazybox panel in a workspace that accepts layout imports. */
export const ReviewLayoutProposal = createContext<((yaml: string) => void) | undefined>(undefined);

export const ApplyWindowArrangement = createContext<((choice: string) => void) | undefined>(undefined);

export const windowArrangements = new Map([
  ['columns', 'Tile windows in equal columns'], ['focus', 'Arrange focused window and stack'],
  ['grid', 'Tile windows in a grid'], ['next-display', 'Move focused window to next display'],
  ['save', 'Save current window positions'], ['restore', 'Restore saved window positions'],
]);
