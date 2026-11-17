// The scenario DSL: a user story, the mocked world it plays in, and a
// timeline of what the person and the agents do. A scenario is a video (the
// real app, recorded) and an end-to-end test (every step checks what it
// changed) at once. See scenarios/README.md.
//
//   export default scenario({
//     title: 'Start a task from a GitHub issue',
//     story: { as: 'a developer', want: 'to start from an issue link', soThat: 'the agent has the issue, its repository and a branch' },
//     build(w) {
//       w.connectGitHub();
//       const payments = w.workspace('Payments', { repos: ['acme/api'] });
//       w.open(payments);
//       return [caption('Paste an issue link into New task'), press('Mod+N'), paste(url), expect.visible(group('From acme/api#812')), …];
//     },
//   });
import { expect as playwrightExpect } from '@playwright/test';
import { step, Target, composer, type Step, type StepContext } from './step.ts';
import { ConversationHandle, type World } from './world.ts';
import type { CaptionPosition } from './overlay.ts';

export * from './step.ts';
export { hears } from './voice.ts';
export { History, World, WorkspaceHandle, ConversationHandle, PullRequestHandle, type IssueSpec, type QuestionSpec, type RunOptions, type TaskSpec } from './world.ts';

// ---- The scenario -----------------------------------------------------------

export type Viewport = 'desktop' | 'phone' | { width: number; height: number };
export type Theme = 'dark' | 'light';

/** "As a … I want … so that …". */
export interface Story { as: string; want: string; soThat: string }

export interface ScenarioSpec {
  title: string;
  story: Story;
  tags?: string[];
  /** Desktop is 1440×900; phone is 390×844. Default desktop. */
  viewport?: Viewport;
  /** Default dark. */
  theme?: Theme;
  /**
   * Builds the mocked world (before the app opens) and returns the timeline.
   * Handles made here (a conversation, a pull request) make steps.
   */
  build(world: World): Step[] | Promise<Step[]>;
}

export interface Scenario extends ScenarioSpec { readonly kind: 'scenario' }

/** Declares a scenario (the default export of a `*.scenario.ts` file). */
export function scenario(spec: ScenarioSpec): Scenario { return { kind: 'scenario', ...spec }; }

// ---- The agent on screen ----------------------------------------------------

/**
 * The conversation on screen (the page's `session`, else the newest): what
 * its agent does. For a named one, use the handle `workspace.conversation()`
 * returns; both have the same steps.
 */
export const agent = new ConversationHandle({ current: true });
/** A conversation found by its title when the step runs (one an orchestrator started, say). */
export const conversation = (title: string) => new ConversationHandle({ title }, title);

// ---- Narration --------------------------------------------------------------

/**
 * A caption over the video, kept until the next one. It stays at least
 * `hold` seconds (default: long enough to read, never under 2.2 s) before the
 * next step. **Bold** is drawn in the accent colour.
 */
export function caption(text: string, options: { hold?: number; position?: CaptionPosition } = {}): Step {
  return { ...step('narration', `caption “${text}”`, async ctx => {
    await ctx.overlay.showCaption(text, options.position || 'bottom');
    const words = text.split(/\s+/).length;
    await ctx.beat(options.hold ?? Math.max(2.2, 0.9 + words * 0.24));
  }), caption: text.replace(/\*\*/g, '') };
}
/** Takes the caption away. */
export const uncaption = (): Step => step('narration', 'caption cleared', async ctx => { await ctx.overlay.clearCaption(); await ctx.beat(0.3); });

/** Rings something on screen, with an optional tag, for `hold` seconds. */
export function highlight(target: Target, options: { label?: string; hold?: number } = {}): Step {
  return step('narration', `highlight ${target.describe}`, async ctx => {
    const locator = target.locate(ctx.page);
    await ctx.check(`${target.describe} is visible`, () => playwrightExpect(locator).toBeVisible({ timeout: ctx.timeout }));
    await locator.scrollIntoViewIfNeeded().catch(() => {});
    // The ring follows the element while it shows (a header that reflows moves it).
    const hold = options.hold ?? 1.8;
    await ctx.beat(0.15);
    for (let shown = 0; ; shown += 0.3) {
      await ctx.overlay.highlight(await locator.boundingBox(), options.label || '');
      if (!ctx.video || shown >= hold) break;
      await ctx.beat(Math.min(0.3, hold - shown));
    }
    await ctx.overlay.highlight(null);
    await ctx.beat(0.25);
  });
}

// ---- The person -------------------------------------------------------------

const KEY_LABELS: Record<string, string> = { Mod: '⌘', Meta: '⌘', Control: '⌃', Alt: '⌥', Shift: '⇧', Enter: '↵', Escape: 'esc', Backspace: '⌫', Tab: '⇥', ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→', Space: 'space' };
const playwrightKeys = (keys: string) => keys.split('+').map(k => k === 'Mod' ? 'ControlOrMeta' : k).join('+');

/** Presses keys, shown on screen as keycaps: `press('Mod+K')`, `press('Enter')`, `press('j')`. */
export function press(keys: string, options: { label?: string } = {}): Step {
  return step('person', `press ${keys}`, async ctx => {
    const caps = keys.split('+').map(k => KEY_LABELS[k] || k.toUpperCase());
    await ctx.overlay.keys(caps);
    await ctx.beat(0.18);
    await ctx.page.keyboard.press(playwrightKeys(keys));
    await ctx.beat(options.label ? 0.6 : 0.45);
  });
}

/** Clicks something, the pointer gliding there first. */
export function click(target: Target, options: { button?: 'left' | 'right'; modifiers?: ('Alt' | 'Shift' | 'ControlOrMeta')[] } = {}): Step {
  return step('person', `${options.button === 'right' ? 'right-click' : 'click'} ${target.describe}`, async ctx => {
    const locator = target.locate(ctx.page);
    await ctx.check(`${target.describe} is visible`, () => playwrightExpect(locator).toBeVisible({ timeout: ctx.timeout }));
    await ctx.overlay.moveToLocator(locator);
    await ctx.beat(0.12);
    await locator.click({ button: options.button, modifiers: options.modifiers, timeout: ctx.timeout });
    await ctx.beat(0.35);
  });
}
/** Opens something's context menu. */
export const rightClick = (target: Target) => click(target, { button: 'right' });

/** Rests the pointer on something (a tooltip, a hover action). */
export function hover(target: Target, seconds = 0.8): Step {
  return step('person', `hover ${target.describe}`, async ctx => {
    const locator = target.locate(ctx.page);
    await ctx.overlay.moveToLocator(locator);
    await locator.hover({ timeout: ctx.timeout });
    await ctx.beat(seconds);
  });
}

/** Scrolls inside something with the wheel, as a trackpad would (`dy` < 0 is up). */
export function scroll(target: Target, dy: number, options: { seconds?: number } = {}): Step {
  return step('person', `scroll ${target.describe} by ${dy}px`, async ctx => {
    const locator = target.locate(ctx.page);
    await ctx.overlay.moveToLocator(locator);
    if (!ctx.video) { await ctx.page.mouse.wheel(0, dy); return; }
    const steps = Math.max(1, Math.round((options.seconds ?? 1.2) * 30));
    for (let i = 0; i < steps; i++) {
      // Fast in the middle, gentle at both ends, like a flick.
      const share = Math.sin(Math.PI * (i + 0.5) / steps) * Math.PI / 2 / steps;
      await ctx.page.mouse.wheel(0, dy * share);
      await new Promise(resolve => setTimeout(resolve, 33));
    }
    await ctx.beat(0.4);
  });
}

/**
 * Types text, a key at a time at a person's pace, into `into` (default: the
 * focused field, else the conversation's message box). A newline is ⇧↵.
 */
export function say(text: string, options: { into?: Target; pace?: number } = {}): Step {
  const preview = text.length > 40 ? text.slice(0, 40) + '…' : text;
  return step('person', `type “${preview}”`, async ctx => {
    const page = ctx.page;
    const editable = await page.evaluate(() => { const e = document.activeElement as HTMLElement | null; return !!e && (e.isContentEditable || ['INPUT', 'TEXTAREA'].includes(e.tagName)); });
    if (options.into || !editable) {
      const field = (options.into || composer()).locate(page);
      await ctx.check(`${(options.into || composer()).describe} is ready`, () => playwrightExpect(field).toBeEditable({ timeout: ctx.timeout }));
      if (!(await field.evaluate(e => e === document.activeElement))) { await ctx.overlay.moveToLocator(field); await field.click(); }
      await ctx.beat(0.2);
    }
    const pace = options.pace ?? 1;
    for (const ch of text) {
      if (ch === '\n') await page.keyboard.press('Shift+Enter'); else await page.keyboard.type(ch);
      // A test types at a fast person's pace, not 1 ms a key. Labelled stopgap: keys 1 ms
      // apart intermittently hit "Maximum update depth exceeded" from AgentWindow's
      // composer onChange (reported with the scenarios); it is an app bug to fix there.
      if (!ctx.video) { await new Promise(resolve => setTimeout(resolve, 12)); continue; }
      let ms = (34 + ctx.random() * 44) / pace;
      if (/[.,;:!?]/.test(ch)) ms += 120 + ctx.random() * 90;
      else if (ch === ' ' && ctx.random() < 0.18) ms += 70;
      await new Promise(resolve => setTimeout(resolve, ms));
    }
    await ctx.beat(0.35);
  });
}

/** Pastes text (as the clipboard delivers it) into `into`, default the focused field. */
export function paste(value: string, options: { into?: Target } = {}): Step {
  return step('person', `paste ${value}`, async ctx => {
    const field = options.into ? options.into.locate(ctx.page) : ctx.page.locator(':focus');
    await ctx.overlay.keys(['⌘', 'V']);
    await ctx.beat(0.2);
    await field.evaluate((element, text) => {
      const data = new DataTransfer(); data.setData('text/plain', text);
      (element as HTMLElement).focus();
      element.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
    }, value);
    await ctx.beat(0.6);
  });
}

/** Waits, as the person reads or the agent works; the scenario's clock moves on. */
export const wait = (seconds: number): Step => step('wait', `wait ${seconds}s`, ctx => ctx.pause(seconds));

/** Opens the app at a path (`/?view=…`), as a link or a bookmark would. */
export const visit = (path: string): Step => step('person', `open ${path}`, async ctx => { await ctx.goto(path); });

/**
 * Reloads the page and checks `until` is on screen within `within` seconds:
 * the app came back where it was, quickly.
 */
export function reload(until: Target, options: { within?: number } = {}): Step {
  const within = options.within ?? 5;
  return step('person', `reload until ${until.describe} (≤ ${within}s)`, async ctx => {
    await ctx.overlay.keys(['⌘', 'R']);
    await ctx.beat(0.3);
    const started = performance.now();
    await ctx.page.reload();
    await ctx.check(`${until.describe} is back within ${within}s`, () => playwrightExpect(until.locate(ctx.page)).toBeVisible({ timeout: within * 1000 }));
    const took = Math.round(performance.now() - started);
    await ctx.check(`reopened in ${took} ms`, async () => { if (took > within * 1000) throw new Error(`took ${took} ms`); });
    await ctx.overlay.restore(); await ctx.overlay.park();
    await ctx.beat(0.5);
  });
}

/** A step of your own (`custom('…', async ({ page, fixture }) => …)`). */
export { custom } from './step.ts';

// ---- Assertions -------------------------------------------------------------

const check = (label: string, assertion: (ctx: StepContext) => Promise<unknown>): Step => step('expect', label, ctx => ctx.check(label, () => assertion(ctx)));

/**
 * What must be true at this point; the scenario fails here otherwise. They
 * take no time in the video.
 */
export const expect = {
  visible: (target: Target) => check(`${target.describe} is visible`, ctx => playwrightExpect(target.locate(ctx.page)).toBeVisible({ timeout: ctx.timeout })),
  hidden: (target: Target) => check(`${target.describe} is gone`, ctx => playwrightExpect(target.locate(ctx.page)).toHaveCount(0, { timeout: ctx.timeout })),
  text: (target: Target, value: string | RegExp) => check(`${target.describe} says “${value}”`, ctx => playwrightExpect(target.locate(ctx.page)).toContainText(value, { timeout: ctx.timeout })),
  exactText: (target: Target, value: string | RegExp | (string | RegExp)[]) => check(`${target.describe} reads “${value}”`, ctx => playwrightExpect(target.locate(ctx.page)).toHaveText(value, { timeout: ctx.timeout })),
  count: (target: Target, count: number) => check(`${count} × ${target.describe}`, ctx => playwrightExpect(target.locate(ctx.page)).toHaveCount(count, { timeout: ctx.timeout })),
  value: (target: Target, value: string | RegExp) => check(`${target.describe} holds “${value}”`, ctx => playwrightExpect(target.locate(ctx.page)).toHaveValue(value, { timeout: ctx.timeout })),
  focused: (target: Target) => check(`${target.describe} has focus`, ctx => playwrightExpect(target.locate(ctx.page)).toBeFocused({ timeout: ctx.timeout })),
  attribute: (target: Target, name: string, value: string | RegExp) => check(`${target.describe} has ${name}=${value}`, ctx => playwrightExpect(target.locate(ctx.page)).toHaveAttribute(name, value, { timeout: ctx.timeout })),
  /** Something the fake Runner received or holds, polled: `expect.runner('the merge', f => f.mutations.some(…))`. */
  runner: (label: string, predicate: (fixture: StepContext['fixture']) => unknown) => check(label, ctx => playwrightExpect.poll(() => !!predicate(ctx.fixture), { timeout: ctx.timeout }).toBe(true)),
  /** The Runner received this call (`expect.call('MergePullRequest', body => body.item.key === 'acme/api#812')`). */
  call: (method: string, matches: (body: any) => boolean = () => true) => check(`the Runner got ${method}`, ctx => playwrightExpect.poll(() => ctx.fixture.calls.some(m => m.method === method && matches(m.body)), { timeout: ctx.timeout }).toBe(true)),
};
