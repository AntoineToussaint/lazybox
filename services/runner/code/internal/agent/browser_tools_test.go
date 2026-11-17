package agent

import (
	"encoding/json"
	"testing"
)

func TestBrowserToolsAreNamedValidAndOnlyReadsReplay(t *testing.T) {
	if len(BrowserTools) != 13 {
		t.Fatalf("the browser tool surface changed (%d tools); update BROWSER.md and both adapters", len(BrowserTools))
	}
	for _, tool := range BrowserTools {
		if !BrowserTool(tool.Name) || !json.Valid(tool.Schema) || tool.Description == "" {
			t.Errorf("%s: malformed", tool.Name)
		}
	}
	for _, name := range []string{"browser_", "browser_evaluate", CommandTool, "browser_openx"} {
		if BrowserTool(name) {
			t.Errorf("%s is not a browser tool", name)
		}
	}
	for name, safe := range map[string]bool{"browser_snapshot": true, "browser_screenshot": true, "browser_wait_for": true, "browser_click": false, "browser_navigate": false, "browser_type": false, "browser_close": false} {
		if ToolReplaySafe(name, json.RawMessage(`{}`)) != safe {
			t.Errorf("%s replay safe should be %v", name, safe)
		}
	}
}

func TestParseBrowserResult(t *testing.T) {
	raw, _ := json.Marshal(BrowserResult{Text: "Page", Image: &BrowserImage{MediaType: "image/jpeg", Data: "AA"}})
	if r := ParseBrowserResult(string(raw)); r.Text != "Page" || r.Image == nil || r.Image.Data != "AA" {
		t.Fatal(r)
	}
	if r := ParseBrowserResult("plain words"); r.Text != "plain words" || r.Image != nil {
		t.Fatal(r)
	}
}
