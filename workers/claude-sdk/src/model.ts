/**
 * A model change, applied between turns. Claude takes a new model at once,
 * even in the middle of a turn; Lazybox says a change applies from the next
 * turn (as Codex does), so one asked for during a turn waits for it to end:
 * the turn finishes on the model it started with. The latest change wins.
 */
import type { Options } from '@anthropic-ai/claude-agent-sdk';

export class ModelSwitch {
  private running = false;
  private applied = Promise.resolve();
  /** Every new message waits for the SDK to acknowledge the selected model. */
  beforeMessage() { return this.applied; }

  private apply(model: string, effort?: Options['effort']) {
    this.applied = this.applied.catch(() => {}).then(() => this.set(model, effort));
    return this.applied;
  }
  private waiting: { model: string; effort?: Options['effort'] } | undefined;
  constructor(private readonly set: (model: string, effort?: Options['effort']) => Promise<void>, private readonly failed: (model: string, error: unknown) => void = () => {}) {}

  /** A message was handed to Claude: a turn runs until turnEnded. */
  turnStarted() { this.running = true; }

  /** Applied now when no turn runs ("acknowledged"), else kept for its end ("accepted"). */
  async change(model: string, effort?: Options['effort']): Promise<'acknowledged' | 'accepted'> {
    if (typeof model !== 'string' || !model || model.length > 256 || /\s/.test(model)) throw new Error('model must be one word: an alias or a model id');
    if (this.running) { this.waiting = { model, effort: effort || this.waiting?.effort }; return 'accepted'; }
    this.waiting = undefined;
    const previous = this.applied;
    const attempt = this.apply(model, effort);
    try { await attempt; } catch (error) {
      if (this.applied === attempt) this.applied = previous;
      throw error;
    }
    return 'acknowledged';
  }

  /** The turn ended (or was interrupted): a change that waited for it applies before the next one. */
  turnEnded(): Promise<void> {
    this.running = false;
    const change = this.waiting;
    this.waiting = undefined;
    if (!change) return Promise.resolve();
    return this.apply(change.model, change.effort).catch(error => this.failed(change.model, error));
  }

  /** The change waiting for the turn to end, if any. */
  get pending() { return this.waiting?.model; }
}
