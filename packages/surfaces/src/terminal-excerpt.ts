import { stripAnsi } from './ansi';

/** Extract only command output between zsh's preexec and precmd markers. */
export function failureExcerpt(snapshot: string, clipped: boolean, status: number): string {
  const startMark = '\x1b]133;C\x07';
  const end = snapshot.indexOf('\x1b]133;D;');
  if (end < 0 || !snapshot.startsWith(`\x1b]133;D;${status}\x07`, end)) return '';
  const start = snapshot.indexOf(startMark);
  if (start < 0 && !clipped || start > end) return '';
  const raw = snapshot.slice(start < 0 ? 0 : start + startMark.length, end);
  const output = stripAnsi(raw).trim().slice(-4000);
  return output ? 'Terminal output (last lines)\n' + output.split('\n').slice(-40).map(line => '> ' + line).join('\n') : '';
}
