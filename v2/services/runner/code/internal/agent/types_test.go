package agent

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestPromptPreservesUserTextAndProvidesEscapedGenericFileReferences(t *testing.T) {
	if Prompt(Operation{Text: "plain"}) != "plain" {
		t.Fatal("plain prompt changed")
	}
	file := File{Name: `quoted "report".pdf`, Path: `/runner/attachments/quoted "report".pdf`, MediaType: "application/pdf", Size: 10}
	text := Prompt(Operation{Text: "Read the report", Files: []File{file}})
	prefix, metadata, ok := strings.Cut(text, "\n\nAttached files available on this Runner (read as needed):\n")
	var decoded []File
	if !ok || prefix != "Read the report" || json.Unmarshal([]byte(metadata), &decoded) != nil || len(decoded) != 1 || decoded[0] != file {
		t.Fatal("generic file references were not delivered as escaped metadata")
	}
}
