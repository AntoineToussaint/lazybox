import { readFileSync } from 'node:fs';

/**
 * Claude's settings_not_in_effect warning as the bundled Claude Code prints
 * it, committed in testdata/settings-not-in-effect.json. The unit tests read
 * only this file; `npm run test:claude-settings` (settings-capture.ts), a gate
 * of its own, checks it is still what the binary prints.
 */
export type Fixture = { claudeCode: string; warnings: Record<string, string> };

/** Where a captured path to the scratch workspace stands in the fixture. */
export const workspace = '<workspace>';
export const fixtureFile = new URL('./testdata/settings-not-in-effect.json', import.meta.url);
export const loadFixture = (): Fixture => JSON.parse(readFileSync(fixtureFile, 'utf8'));
