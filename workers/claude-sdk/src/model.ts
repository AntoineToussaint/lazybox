/**
 * A model change, applied between turns. Claude takes a new model at once,
 * even in the middle of a turn; Lazybox says a change applies from the next
 * turn (as Codex does), so one asked for during a turn waits for it to end:
 * the turn finishes on the model it started with. The latest change wins.
 */
export class ModelSwitch {
  private running = false;
  private waiting: string | undefined;
  constructor(private readonly set: (model: string) => Promise<void>, private readonly failed: (model: string, error: unknown) => void = () => {}) {}

  /** A message was handed to Claude: a turn runs until turnEnded. */
  turnStarted() { this.running = true; }

  /** Applied now when no turn runs ("acknowledged"), else kept for its end ("accepted"). */
  async change(model: string): Promise<'acknowledged' | 'accepted'> {
    if (typeof model !== 'string' || !model || model.length > 256 || /\s/.test(model)) throw new Error('model must be one word: an alias or a model id');
    if (this.running) { this.waiting = model; return 'accepted'; }
    this.waiting = undefined;
    await this.set(model);
    return 'acknowledged';
  }

  /** The turn ended (or was interrupted): a change that waited for it applies before the next one. */
  turnEnded(): Promise<void> {
    this.running = false;
    const model = this.waiting;
    this.waiting = undefined;
    if (!model) return Promise.resolve();
    return this.set(model).catch(error => this.failed(model, error));
  }

  /** The change waiting for the turn to end, if any. */
  get pending() { return this.waiting; }
}
