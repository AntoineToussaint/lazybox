// Types for page-errors.mjs, for TypeScript callers (scenarios).
import type { BrowserContext, Page } from './playwright.mjs';

export interface PageErrorOptions {
  /** Prefixes each message, for several pages pooled into one list. */
  label?: string;
  /** Rewrites each message as it is recorded (after `label`). */
  format?: (message: string) => string;
  /** Sees each error as it comes, after it is recorded; one the app then handles is retracted from the list only. */
  onError?: (message: string, stack: string) => void;
}

/** Collects `page`'s uncaught errors and unhandled rejections; call before its first navigation. */
export function collectPageErrors(page: Page, errors?: string[], options?: PageErrorOptions): Promise<string[]>;

/** The same for every page of `context`, popups included; call before it opens a page. */
export function collectContextPageErrors(context: BrowserContext, errors?: string[], options?: PageErrorOptions): Promise<string[]>;

/** Waits until everything the collected pages dispatched so far has reached `errors` (or been retracted). */
export function drainPageErrors(errors: string[]): Promise<string[]>;
