package commands

// KindHook marks a hook run (POWER.md): a program the Runner started for a
// user's hook, or, for a webhook, a pipe holding the request's log. It
// belongs to the event's workspace but is not one of its runs, so no
// conversation's console lists it; Settings → Actions & hooks does, and
// Processes while it runs.
const KindHook = "hook"

// KindRunner marks a command the Runner runs for the person outside any
// workspace or conversation, such as installing the agent browser's Chromium
// from Settings → Browser. Its console shows where it was started.
const KindRunner = "runner"
