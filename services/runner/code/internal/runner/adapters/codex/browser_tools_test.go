package codex

import (
	"encoding/json"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A coding thread gets the browser tools when the Runner offers its browser;
// they run without a per-call approval (the browser asks per site), and a
// screenshot reaches Codex as an image.
func TestBrowserToolsAreOfferedRunWithoutApprovalAndReturnImages(t *testing.T) {
	names := func(tools []map[string]any) map[string]bool {
		out := map[string]bool{}
		for _, t := range tools {
			out[t["name"].(string)] = true
		}
		return out
	}
	with, without := names(dynamicTools("coding", true)), names(dynamicTools("coding", false))
	for _, tool := range agent.BrowserTools {
		if !with[tool.Name] || without[tool.Name] {
			t.Errorf("%s offered %v with the browser, %v without", tool.Name, with[tool.Name], without[tool.Name])
		}
		if !json.Valid(tool.Schema) {
			t.Errorf("%s: invalid schema", tool.Name)
		}
		if toolApproval(tool.Name) != "" {
			t.Errorf("%s asks for a per-call approval", tool.Name)
		}
		// A resumed thread may hold them whatever this Runner offers now.
		if !toolOffered("coding", tool.Name) || toolOffered("help", tool.Name) {
			t.Errorf("%s: offered to the wrong purposes", tool.Name)
		}
	}
	if !with[agent.CommandTool] || names(dynamicTools("help", true))["browser_open"] {
		t.Fatal("browser tools changed what else is offered")
	}
	raw, _ := json.Marshal(agent.BrowserResult{Text: "Screenshot of Docs", Image: &agent.BrowserImage{MediaType: "image/jpeg", Data: "AAAA"}})
	items := browserContent(string(raw))
	if len(items) != 2 || items[0].(map[string]string)["text"] != "Screenshot of Docs" || items[1].(map[string]string)["imageUrl"] != "data:image/jpeg;base64,AAAA" {
		t.Fatalf("%v", items)
	}
	if plain := browserContent("Page text"); len(plain) != 1 || plain[0].(map[string]string)["text"] != "Page text" {
		t.Fatalf("%v", plain)
	}
}
