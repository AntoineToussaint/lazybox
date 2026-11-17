import type { CommandStatus } from '@lazybox/protocol';

/** Pure: a conversation's commands for the phone's strip — running first, then the newest finished; at most `limit`. */
export function phoneCommands(commands: readonly CommandStatus[], limit = 6): CommandStatus[] {
  const running = commands.filter(command => !command.done);
  const finished = commands.filter(command => command.done).reverse();
  return [...running, ...finished].slice(0, limit);
}
