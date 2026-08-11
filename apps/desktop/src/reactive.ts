export type MutationState = "pending" | "confirmed" | "rejected";

export interface MutationRecord<T> {
  readonly id: number;
  readonly workspaceKey: string;
  readonly operation: string;
  readonly command: string;
  readonly previous: T;
  readonly requested: T;
  state: MutationState;
  message: string;
}

function mutationKey(workspaceKey: string, operation: string): string {
  return `${workspaceKey}\0${operation}`;
}

/** Session-retained mutation state, with one optimistic head per operation. */
export class MutationLedger<T> {
  /** Cap the retained log so a long session cannot grow it (and the messages
   * DOM patched from it) without bound. The newest records are kept. */
  private static readonly MAX_RECORDS = 200;
  private sequence = 0;
  private readonly pending = new Map<string, MutationRecord<T>>();
  private readonly records: MutationRecord<T>[] = [];

  private retain(record: MutationRecord<T>): void {
    this.records.push(record);
    const overflow = this.records.length - MutationLedger.MAX_RECORDS;
    if (overflow > 0) {
      this.records.splice(0, overflow);
    }
  }

  begin(
    workspaceKey: string,
    operation: string,
    command: string,
    previous: T,
    requested: T,
    message: string,
  ): MutationRecord<T> {
    const key = mutationKey(workspaceKey, operation);
    const prior = this.pending.get(key);
    if (prior !== undefined) {
      prior.state = "rejected";
      prior.message = "Superseded by a newer request.";
    }
    const record: MutationRecord<T> = {
      id: ++this.sequence,
      workspaceKey,
      operation,
      command,
      previous,
      requested,
      state: "pending",
      message,
    };
    this.pending.set(key, record);
    this.retain(record);
    return record;
  }

  optimistic(workspaceKey: string, operation: string): T | undefined {
    return this.pending.get(mutationKey(workspaceKey, operation))?.requested;
  }

  confirm(workspaceKey: string, operation: string, message: string): void {
    const key = mutationKey(workspaceKey, operation);
    const record = this.pending.get(key);
    if (record === undefined) {
      return;
    }
    record.state = "confirmed";
    record.message = message;
    this.pending.delete(key);
  }

  confirmValue(
    workspaceKey: string,
    operation: string,
    committed: T,
    equals: (left: T, right: T) => boolean = Object.is,
  ): boolean {
    const record = this.pending.get(mutationKey(workspaceKey, operation));
    if (record === undefined || !equals(record.requested, committed)) {
      return false;
    }
    this.confirm(workspaceKey, operation, "Confirmed by the daemon.");
    return true;
  }

  /**
   * Roll back a single optimistic mutation for a rejected command. The
   * `CommandRejected` wire event names only the command *type*, never the
   * workspace it targeted (crates/ipc), and the daemon rejects commands in the
   * order it received them. So the oldest still-pending mutation for that
   * command is the one this rejection refers to — rejecting only it (rather
   * than every workspace that happens to share the command name) keeps an
   * unrelated workspace's in-flight mutation from being rolled back. Returns
   * the rejected record wrapped in an array, or an empty array if none match.
   */
  rejectCommand(command: string, message: string): MutationRecord<T>[] {
    for (const [key, record] of this.pending) {
      if (record.command !== command) {
        continue;
      }
      record.state = "rejected";
      record.message = message;
      this.pending.delete(key);
      return [record];
    }
    return [];
  }

  reject(
    workspaceKey: string,
    operation: string,
    message: string,
  ): MutationRecord<T> | undefined {
    const key = mutationKey(workspaceKey, operation);
    const record = this.pending.get(key);
    if (record !== undefined) {
      record.state = "rejected";
      record.message = message;
      this.pending.delete(key);
    }
    return record;
  }

  recordOutcome(
    workspaceKey: string,
    operation: string,
    state: Exclude<MutationState, "pending">,
    message: string,
    value: T,
  ): MutationRecord<T> {
    const record: MutationRecord<T> = {
      id: ++this.sequence,
      workspaceKey,
      operation,
      command: operation,
      previous: value,
      requested: value,
      state,
      message,
    };
    this.retain(record);
    return record;
  }

  history(workspaceKey?: string): readonly MutationRecord<T>[] {
    return workspaceKey === undefined
      ? this.records
      : this.records.filter((record) => record.workspaceKey === workspaceKey);
  }

  dispose(): void {
    this.pending.clear();
    this.records.length = 0;
  }
}

/** Independent monotonically increasing request generations. */
export class RequestGenerations {
  private readonly values = new Map<string, number>();

  begin(scope: string): number {
    const generation = (this.values.get(scope) ?? 0) + 1;
    this.values.set(scope, generation);
    return generation;
  }

  isCurrent(scope: string, generation: number): boolean {
    return this.values.get(scope) === generation;
  }

  cancel(scope: string): void {
    this.begin(scope);
  }

  dispose(): void {
    this.values.clear();
  }
}
