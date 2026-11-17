// The step: one beat of a scenario, run against the real app (Playwright) and
// the fake Runner. Every builder in dsl.ts and world.ts returns one. A step
// knows how to describe itself (for the report) and how to run.
import type { Locator, Page } from '@playwright/test';
import type { RunnerFixture } from '../../scripts/fixtures/runner-ui.mjs';
import type { World } from './world.ts';
import type { ScenarioClock } from './clock.ts';
import type { Overlay } from './overlay.ts';

export type StepKind = 'person' | 'agent' | 'world' | 'narration' | 'expect' | 'wait';

export interface Step {
  kind: StepKind;
  /** What the step does, in a few words, for the report: `click button "Send"`. */
  label: string;
  /** A caption step's text: it starts a chapter of the contact sheet. */
  caption?: string;
  run(ctx: StepContext): Promise<void>;
}

/** What a step runs against. */
export interface StepContext {
  page: Page;
  fixture: RunnerFixture;
  world: World;
  clock: ScenarioClock;
  overlay: Overlay;
  /** True when the run records a video: pauses are real; false runs it as a fast test. */
  video: boolean;
  /** A deterministic random number in [0, 1), seeded by the scenario's name. */
  random(): number;
  /** The default wait for an assertion, in milliseconds. */
  timeout: number;
  /**
   * A pause the viewer sees: real time in a video, nothing in a test. The
   * scenario's clock advances by it either way, so times read the same.
   */
  pause(seconds: number): Promise<void>;
  /**
   * Real time passing, in a test too, the clock ticking each second: for what
   * the app measures by its own polls (a run long enough to take the console).
   */
  hold(seconds: number): Promise<void>;
  /** A pause of video only (a beat between actions); the clock does not move. */
  beat(seconds: number): Promise<void>;
  /** Records a named check in the report; throws (failing the step) when it fails. */
  check(description: string, assertion: () => Promise<unknown>): Promise<void>;
  /** Lets the app see a change of the Runner's state now rather than at its next poll. */
  nudge(): Promise<void>;
  /** Navigates the recorded page, keeping the overlay's caption. */
  goto(path: string): Promise<void>;
}

/** Makes a step. */
export const step = (kind: StepKind, label: string, run: (ctx: StepContext) => Promise<void>): Step => ({ kind, label, run });

/** A custom step, for what the DSL does not say yet: `custom('scroll to the top', async ({ page }) => …)`. */
export const custom = (label: string, run: (ctx: StepContext) => Promise<void>): Step => step('person', label, run);

// ---- Targets ----------------------------------------------------------------

type Role = Parameters<Page['getByRole']>[0];
type Scope = Page | Locator;

/**
 * Something on screen, found the way a person (and an accessibility tool)
 * finds it: by role and name first. Build one with `button('Send')`,
 * `role('tab', 'Changes')`, `text('Fix refund rounding')` or `css('.pr-pill')`,
 * then narrow it: `button('Send').in(dialog('New task'))`, `.first()`,
 * `.nth(1)`, `.withText('…')`.
 */
export class Target {
  constructor(readonly describe: string, private readonly find: (scope: Scope) => Locator) {}
  /** The Playwright locator in a page (or inside another locator). */
  locate(scope: Scope): Locator { return this.find(scope); }
  /** This target, looked for inside `parent` only. */
  in(parent: Target): Target { return new Target(`${this.describe} in ${parent.describe}`, scope => this.find(parent.locate(scope))); }
  first(): Target { return new Target(`first ${this.describe}`, scope => this.find(scope).first()); }
  last(): Target { return new Target(`last ${this.describe}`, scope => this.find(scope).last()); }
  nth(index: number): Target { return new Target(`${this.describe} #${index + 1}`, scope => this.find(scope).nth(index)); }
  /** This target, or `other` when this one is not there (a field whose name differs by layout). */
  or(other: Target): Target { return new Target(`${this.describe} (or ${other.describe})`, scope => this.find(scope).or(other.locate(scope)).first()); }
  /** Only the matches that contain this text. */
  withText(text: string | RegExp): Target { return new Target(`${this.describe} with “${text}”`, scope => this.find(scope).filter({ hasText: text })); }
}

/** An element by its ARIA role and accessible name (exact unless a RegExp). */
export const role = (kind: Role, name?: string | RegExp, options: { exact?: boolean } = {}): Target =>
  new Target(name === undefined ? kind : `${kind} “${name}”`, scope => scope.getByRole(kind, name === undefined ? {} : { name, exact: typeof name === 'string' ? options.exact ?? true : undefined }));
export const button = (name: string | RegExp) => role('button', name);
export const link = (name: string | RegExp) => role('link', name);
export const tab = (name: string | RegExp) => role('tab', name);
export const option = (name: string | RegExp) => role('option', name);
export const menuitem = (name: string | RegExp) => role('menuitem', name);
export const textbox = (name: string | RegExp) => role('textbox', name);
export const dialog = (name: string | RegExp) => role('dialog', name);
export const region = (name: string | RegExp) => role('region', name);
export const group = (name: string | RegExp) => role('group', name);
export const checkbox = (name: string | RegExp) => role('checkbox', name);
export const heading = (name: string | RegExp) => role('heading', name);
/** Visible text. */
export const text = (value: string | RegExp, options: { exact?: boolean } = {}) => new Target(`text “${value}”`, scope => scope.getByText(value, options));
/** A form control by its label. */
export const label = (value: string | RegExp) => new Target(`field “${value}”`, scope => scope.getByLabel(value, { exact: typeof value === 'string' }));
/** A CSS selector: for what has no role or name (a pill, a row's badge). */
export const css = (selector: string, describe = selector) => new Target(describe, scope => scope.locator(selector));
/** The message box of the conversation on screen. */
export const composer = (): Target => new Target('the message box', scope => scope.getByRole('textbox', { name: 'Message', exact: true }).first());
/** The conversation's transcript. */
export const transcript = (): Target => css('.messages', 'the conversation');
