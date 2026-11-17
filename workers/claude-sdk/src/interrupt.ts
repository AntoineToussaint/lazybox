/**
 * A turn the person interrupted ends with Claude's "error during execution"
 * diagnostic ("[ede_diagnostic] result_type=user … stop_reason=tool_use").
 * That failure only reports the interrupt; it must not fault the conversation.
 * Account limits and sign-in problems still count.
 */
export function droppedOnInterrupt(event: { type?: string; status?: string }): boolean {
  return event.type === 'failure' && (event.status === 'provider_error' || event.status === 'unknown' || event.status === 'error_during_execution');
}

/**
 * Which turn the person interrupted. An interrupt belongs to the turn running
 * when it was asked, or, between a message and its turn's first event, to the
 * turn that message starts; to no other. A flag that outlived its turn (the
 * interrupt failed, or the turn had already ended) would drop the next turn's
 * real failure and end that turn as interrupted.
 */
export class TurnInterrupts {
  // Turns started so far: the id of the running one, or of the last one.
  private turn = 0;
  private running = false;
  // A message was delivered while no turn ran; its turn has not started yet.
  private awaiting = false;
  // The turn an interrupt was asked for.
  private target: number | undefined;

  /** A turn runs, or a message was handed to the SDK and its turn is about to start. */
  get inTurn(): boolean { return this.running || this.awaiting; }

  /** A message was handed to the SDK. */
  delivered(): void {
    if (!this.running) this.awaiting = true;
  }

  /**
   * Marks the turn an interrupt asked now applies to, and returns how to take
   * the mark back if the interrupt fails. Nothing is marked when no turn runs
   * or is about to: the interrupt has nothing to stop.
   */
  mark(): () => void {
    const previous = this.target;
    const turn = this.running ? this.turn : this.awaiting ? this.turn + 1 : undefined;
    if (turn === undefined) return () => {};
    this.target = turn;
    return () => { if (this.target === turn) this.target = previous; };
  }

  /** Applies the interrupt to one event the worker is about to send; false drops it. */
  pass(event: { type?: string; status?: string }): boolean {
    if (event.type === 'turn_started') {
      this.turn++;
      this.running = true;
      this.awaiting = false;
      if (this.target !== undefined && this.target < this.turn) this.target = undefined;
      return true;
    }
    // Events before a turn's first one belong to the turn about to start.
    const current = this.running ? this.turn : this.turn + 1;
    const interrupted = this.target === current;
    if (interrupted && droppedOnInterrupt(event)) return false;
    if (event.type === 'turn_ended') {
      if (!this.running) this.turn = current; // A turn that ended without a first event.
      if (interrupted) event.status = 'interrupted';
      this.running = false;
      this.awaiting = false;
      this.target = undefined;
    }
    return true;
  }
}

/** Interrupts the running turn; a failed interrupt leaves no mark behind. */
export async function interruptTurn(interrupts: TurnInterrupts, interrupt: () => Promise<unknown>): Promise<void> {
  const undo = interrupts.mark();
  try {
    await interrupt();
  } catch (error) {
    undo();
    throw error;
  }
}
