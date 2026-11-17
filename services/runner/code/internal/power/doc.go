// Package power is Lazybox's safe extensibility for power users (POWER.md):
// one declarative file of actions and hooks, never code that runs inside the
// app.
//
// Actions are named sequences of steps (send a prompt or snippet, open a
// panel, a URL or a file, run a workspace action or a command, change a
// conversation's permission, wait for an agent) that the app offers in its
// menus and runs with the owner's own rules: a workspace action still needs
// its scripts approved, a read-only conversation still runs no command.
//
// Hooks are programs, shell lines or webhooks the Runner starts when
// something happens (an agent needs input, a turn ends, a pull request's
// checks fail, a person accepts a mission's work). They run as managed
// commands, visible in Processes with their output, and hold no Runner
// credential unless the file grants them one, scoped like a paired phone's.
//
// The package parses and validates the file (with line numbers), reloads it
// when it changes, matches events against hooks, and brokers the requests
// the CLI makes of an open window (`lazybox open`, `lazybox shot`).
package power
