package runner

import (
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// The agent browser in a coding agent's briefing (BROWSER.md): whether it has
// the browser and how to use it, or, when it has not, how the person gives
// it one. It is a section of its own, beside the command briefing, and it is
// per conversation: when it changes (the person gave the agent the browser,
// or turned it off) the agent hears it with its next message, like any
// change of the shared instructions.

const browserHasBriefing = `## Your browser
You have a browser: Lazybox's agent browser, the ` + "`browser_*`" + ` tools (browser_open, browser_snapshot, browser_click, browser_type, browser_screenshot…). Use it whenever a task needs a web page: documentation, a staging site, or the app you run with ` + "`lazybox run`" + ` (its localhost port is reachable). The person watches every step live in the Agent browser panel beside the chat and can take over.
- Prefer it to a headless browser of your own (Playwright or Puppeteer scripts, curl to scrape): the person sees nothing of those.
- The first visit to a site asks the person; give a reason. Never type a password: ask the person to take over.
- If browser_open says Chromium is not installed, tell the person they can install it with one click in Settings → Browser; do not install it yourself.`

const browserMissingBriefing = `## Your browser
You do not have Lazybox's agent browser in this conversation: {{WHY}}. If a task needs a browser, say so and ask the person to choose “Give {{AGENT}} the browser” in this conversation's menu (or with ⌘K): you restart with the browser and the conversation continues; they watch it in the Agent browser panel. Do not drive a headless browser of your own (Playwright or Puppeteer scripts) without telling the person.`

const browserOffBriefing = `## Your browser
Lazybox's agent browser is off (Settings → Browser), so you have none. If a task needs a browser, say so and ask the person to turn it on there. Do not drive a headless browser of your own (Playwright or Puppeteer scripts) without telling the person.`

// browserBriefing is the browser section of a coding agent's briefing; has
// says whether its agent has the browser tools. Empty where the browser does
// not apply: a Runner without the service, assistants, ACP agents.
func (b *Bench) browserBriefing(provider, purpose, workspace string, has bool) string {
	if b.Browser == nil || !b.Browser.Available() || agent.IsAssistant(purpose) || !browserProvider(provider) {
		return ""
	}
	if !b.Browser.Enabled() {
		return browserOffBriefing
	}
	off := b.Browser.WorkspaceOff(workspace)
	if has && !off {
		return browserHasBriefing
	}
	why := "it started before Lazybox's browser was available to it"
	if off {
		why = "the person turned it off for this workspace"
	}
	name := "the agent"
	if provider != "" {
		name = strings.ToUpper(provider[:1]) + provider[1:]
	}
	return strings.NewReplacer("{{WHY}}", why, "{{AGENT}}", name).Replace(browserMissingBriefing)
}
