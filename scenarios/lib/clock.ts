// The scenario's clock: one virtual time shared by the fake Runner (this
// process's Date.now) and the page (Playwright's fixed clock), so "2 min ago",
// a turn's duration and a timestamp read the same on every run, in a test and
// in a video. Time moves only when the scenario says so (a pause, an agent
// thinking); the page sees it move at each step and once a second during a
// pause.

/** Tuesday 17 November 2026, 15:42 UTC: the docs screenshots' moment too. */
export const SCENARIO_START = Date.UTC(2026, 10, 17, 15, 42, 0);

const realNow = Date.now.bind(Date);
let offset = 0;
let patched = false;

export class ScenarioClock {
  now: number;
  constructor(readonly start = SCENARIO_START) {
    this.now = start;
    if (!patched) { patched = true; Date.now = () => realNow() + offset; }
    this.syncProcess();
  }
  /** Moves the scenario forward. */
  advance(ms: number) { this.now += Math.max(0, Math.round(ms)); this.syncProcess(); }
  /** What the fake Runner records next happened at the scenario's present. */
  syncProcess() { offset = this.now - realNow(); }
  /** ISO time, as the Runner stamps journal records. */
  iso(at = this.now) { return new Date(at).toISOString(); }
  ms(at = this.now) { return String(at); }
}
