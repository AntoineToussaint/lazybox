package claude

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestMessageSendsContentBlocksInOrder(t *testing.T) {
	paste := filepath.Join(t.TempDir(), "paste.txt")
	if err := os.WriteFile(paste, []byte("a\nb\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	op := agent.Operation{ID: "m", Kind: agent.Message, Text: "Look", Context: "Updated instructions", Files: []agent.File{
		{Name: "shot.png", MediaType: "image/png", Path: "/r/shot.png", Size: 10, Delivery: agent.DeliverImage},
		{Name: "Pasted text.txt", MediaType: "text/plain", Path: paste, Size: 4, Delivery: agent.DeliverText},
		{Name: "report.pdf", MediaType: "application/pdf", Path: "/r/report.pdf", Size: 2048, Delivery: agent.DeliverFile},
	}, Images: []agent.Image{{MediaType: "image/png", Data: "c2hvdA=="}}}

	// A worker that takes parts: one block each, in order, and no paths or
	// duplicated text and images beside them.
	data, err := json.Marshal(message(op, true))
	if err != nil {
		t.Fatal(err)
	}
	var wire struct {
		Text    string              `json:"text"`
		Context string              `json:"context"`
		Images  []agent.Image       `json:"images"`
		Files   []agent.File        `json:"files"`
		Parts   []map[string]string `json:"parts"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		t.Fatal(err)
	}
	if wire.Text != "" || wire.Images != nil || wire.Files != nil || wire.Context != "Updated instructions" || len(wire.Parts) != 4 {
		t.Fatalf("wire: %s", data)
	}
	if wire.Parts[0]["text"] != "Look" || wire.Parts[1]["type"] != "image" || wire.Parts[1]["data"] != "c2hvdA==" || wire.Parts[1]["mediaType"] != "image/png" ||
		wire.Parts[2]["text"] != "Attached file \"Pasted text.txt\" (2 lines):\n```\na\nb\n```" || !strings.Contains(wire.Parts[3]["text"], `"/r/report.pdf"`) {
		t.Fatalf("parts: %+v", wire.Parts)
	}

	// A worker from before parts: the same content as text, the images beside.
	data, err = json.Marshal(message(op, false))
	if err != nil {
		t.Fatal(err)
	}
	wire.Parts, wire.Images = nil, nil
	if err := json.Unmarshal(data, &wire); err != nil {
		t.Fatal(err)
	}
	if wire.Parts != nil || len(wire.Images) != 1 || !strings.HasPrefix(wire.Text, "Look\n\nAttached file \"Pasted text.txt\" (2 lines):\n```\na\nb\n```\n\nAttached file \"report.pdf\"") {
		t.Fatalf("fallback: %s", data)
	}
}
