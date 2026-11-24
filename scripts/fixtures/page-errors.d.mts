// Types for page-errors.mjs, for TypeScript callers (scenarios).
import type { BrowserContext, Page } from './playwright.mjs';

export interface PageErrorOptions {
  /** Prefixes each message, for several pages pooled into one list. */
  label?: string;
  /** Sees each error as it comes, after it is recorded. */
  onError?: (message: string, stack: string) => void;
}

/** Collects `page`'s uncaught errors and unhandled rejections; call before its first navigation. */
export function collectPageErrors(page: Page, errors?: string[], options?: PageErrorOptions): Promise<string[]>;

/** The same for every page of `context`, popups included; call before it opens a page. */
export function collectContextPageErrors(context: BrowserContext, errors?: string[], options?: PageErrorOptions): Promise<string[]>;
