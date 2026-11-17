package acp

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestPromptDeliversAttachmentsAsACPBlocksInOrder(t *testing.T) {
	paste := filepath.Join(t.TempDir(), "paste.go")
	if err := os.WriteFile(paste, []byte("package main\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	op := agent.Operation{Text: "Look", Files: []agent.File{
		{Name: "Pasted text.go", MediaType: "text/plain", Path: paste, Size: 13, Delivery: agent.DeliverText},
		{Name: "shot.png", MediaType: "image/png", Path: "/r/shot.png", Size: 10, Delivery: agent.DeliverImage},
		{Name: "report.pdf", MediaType: "application/pdf", Path: "/r/report.pdf", Size: 2048, Delivery: agent.DeliverFile},
	}, Images: []agent.Image{{MediaType: "image/png", Data: "c2hvdA=="}}}
	types := func(blocks []any) string {
		out := []string{}
		for _, b := range blocks {
			out = append(out, b.(map[string]any)["type"].(string))
		}
		return strings.Join(out, ",")
	}
	// An agent that takes images and embedded context: the text as an
	// embedded resource, the image as an image, the PDF as a resource link.
	rich := &session{state: durable{Agent: agentFacts{Image: true, Embedded: true}}}
	blocks := rich.prompt(op, "")
	if types(blocks) != "text,resource,image,resource_link" {
		t.Fatalf("rich agent: %s", types(blocks))
	}
	if blocks[0].(map[string]any)["text"] != "Look" {
		t.Fatalf("the person's text is not first: %+v", blocks[0])
	}
	resource := blocks[1].(map[string]any)["resource"].(map[string]any)
	if resource["text"] != "package main\n" || resource["uri"] != "file://"+filepath.ToSlash(paste) || resource["mimeType"] != "text/plain" {
		t.Fatalf("embedded text: %+v", resource)
	}
	link := blocks[3].(map[string]any)
	if link["uri"] != "file:///r/report.pdf" || link["name"] != "report.pdf" || link["mimeType"] != "application/pdf" {
		t.Fatalf("resource link: %+v", link)
	}
	// The baseline agent (text and resource links only): the text inline, in
	// a fenced block, and an image it cannot see as a link to the file.
	plain := &session{state: durable{}}
	blocks = plain.prompt(op, "")
	if types(blocks) != "text,text,resource_link,resource_link" {
		t.Fatalf("baseline agent: %s", types(blocks))
	}
	if blocks[1].(map[string]any)["text"] != "Attached file \"Pasted text.go\" (1 line):\n```go\npackage main\n```" || blocks[2].(map[string]any)["name"] != "shot.png" {
		t.Fatalf("baseline blocks: %+v", blocks)
	}
}
