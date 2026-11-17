package agent

import (
	"encoding/json"
	"strings"
)

// The agent browser's tools (BROWSER.md). Codex gets them as dynamic tools
// with these names; the Claude worker exposes the same tools on its in-process
// `lazybox_browser` MCP server (mcp__lazybox_browser__browser_*) and routes
// each call to the Runner under the same name. The Runner runs them against
// the calling conversation's own browser and nothing else.

// BrowserToolPrefix starts every browser tool's name.
const BrowserToolPrefix = "browser_"

// BrowserToolDef is one browser tool as offered to an agent.
type BrowserToolDef struct {
	Name, Description string
	Schema            json.RawMessage
}

const refProp = `"ref":{"type":"string","description":"The element's ref from the latest browser_snapshot, such as e12."},"element":{"type":"string","description":"A few words naming the element for the person watching, such as \"Sign in button\"."}`

// BrowserTools is the browser tool surface, in the order agents see it.
var BrowserTools = []BrowserToolDef{
	{"browser_open", "Open your browser (a Chromium the person watches live beside the chat), optionally at a URL. The first visit to each site asks the person unless it is allowed. Returns the page's title, URL and a compact snapshot with element refs.",
		json.RawMessage(`{"type":"object","properties":{"url":{"type":"string","description":"An http or https URL."},"reason":{"type":"string","description":"One short sentence for the person: why you need this site."}},"additionalProperties":false}`)},
	{"browser_navigate", "Go to a URL in your browser. A site not visited yet asks the person first, with your reason. Returns the page's title, URL and a compact snapshot.",
		json.RawMessage(`{"type":"object","properties":{"url":{"type":"string","description":"An http or https URL."},"reason":{"type":"string","description":"One short sentence for the person: why you need this site."}},"required":["url"],"additionalProperties":false}`)},
	{"browser_snapshot", "Read the current page as a compact accessibility tree with element refs ([ref=e12]) to click, type into or select. Refs are valid until the next snapshot.",
		json.RawMessage(`{"type":"object","properties":{"maxChars":{"type":"integer","minimum":500,"maximum":40000,"description":"Cap on the snapshot's length; 8000 by default."}},"additionalProperties":false}`)},
	{"browser_click", "Click an element by its ref. Returns what happened and a fresh snapshot.",
		json.RawMessage(`{"type":"object","properties":{` + refProp + `},"required":["ref"],"additionalProperties":false}`)},
	{"browser_type", "Type text into a field by its ref, replacing its value; submit presses Enter. Never type passwords or other credentials: ask the person to take over the browser and type them.",
		json.RawMessage(`{"type":"object","properties":{` + refProp + `,"text":{"type":"string"},"submit":{"type":"boolean","description":"Press Enter after typing."}},"required":["ref","text"],"additionalProperties":false}`)},
	{"browser_select", "Choose one or more options of a select element by its ref.",
		json.RawMessage(`{"type":"object","properties":{` + refProp + `,"values":{"type":"array","items":{"type":"string"},"minItems":1,"description":"Option values or labels."}},"required":["ref","values"],"additionalProperties":false}`)},
	{"browser_press", "Press a key in the page, such as Enter, Escape, Tab or ArrowDown.",
		json.RawMessage(`{"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}`)},
	{"browser_scroll", "Scroll the page (or an element into view by its ref).",
		json.RawMessage(`{"type":"object","properties":{"direction":{"type":"string","enum":["up","down","left","right"]},"amount":{"type":"number","minimum":1,"description":"CSS pixels; most of a screen by default."},"ref":{"type":"string","description":"Scroll this element into view instead."}},"additionalProperties":false}`)},
	{"browser_screenshot", "Take a screenshot of the page; it is attached to the conversation as an image. Prefer browser_snapshot to read a page: it is far cheaper.",
		json.RawMessage(`{"type":"object","properties":{"fullPage":{"type":"boolean","description":"The whole page rather than the visible part."}},"additionalProperties":false}`)},
	{"browser_back", "Go back one page in your browser's history.", json.RawMessage(`{"type":"object","properties":{},"additionalProperties":false}`)},
	{"browser_forward", "Go forward one page in your browser's history.", json.RawMessage(`{"type":"object","properties":{},"additionalProperties":false}`)},
	{"browser_wait_for", "Wait until text appears, text disappears, or an element by ref becomes visible.",
		json.RawMessage(`{"type":"object","properties":{"text":{"type":"string"},"textGone":{"type":"string"},"ref":{"type":"string"},"timeoutSeconds":{"type":"number","minimum":1,"maximum":60}},"additionalProperties":false}`)},
	{"browser_close", "Close your browser when you are done with it.", json.RawMessage(`{"type":"object","properties":{},"additionalProperties":false}`)},
}

// BrowserTool reports whether name is one of the browser tools.
func BrowserTool(name string) bool {
	if !strings.HasPrefix(name, BrowserToolPrefix) {
		return false
	}
	for _, t := range BrowserTools {
		if t.Name == name {
			return true
		}
	}
	return false
}

// BrowserResult is what every browser tool returns to its adapter: text for
// the agent and, for a screenshot, the image. Adapters unwrap it into their
// provider's content blocks.
type BrowserResult struct {
	Text  string        `json:"text"`
	Image *BrowserImage `json:"image,omitempty"`
}

// BrowserImage is a screenshot, base64.
type BrowserImage struct {
	MediaType string `json:"mediaType"`
	Data      string `json:"data"`
}

// ParseBrowserResult reads a browser tool's result; text that is not one is
// returned as plain text.
func ParseBrowserResult(raw string) BrowserResult {
	var r BrowserResult
	if json.Unmarshal([]byte(raw), &r) == nil && (r.Text != "" || r.Image != nil) {
		return r
	}
	return BrowserResult{Text: raw}
}

// browserReplaySafe: reading a page again has no effect a second run could
// repeat. Everything else (navigating, clicking, typing) may.
func browserReplaySafe(name string) bool {
	switch name {
	case "browser_snapshot", "browser_screenshot", "browser_wait_for":
		return true
	}
	return false
}
