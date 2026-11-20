package runner

import (
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

func TestWorkflowReviewUsesRepeatedHumanEvidenceWithinWorkspace(t *testing.T) {
	log, err := work.OpenPrompts(filepath.Join(t.TempDir(), "prompts"))
	if err != nil {
		t.Fatal(err)
	}
	defer log.Close()
	send := func(ws, conversation, kind, text string) {
		t.Helper()
		source := work.PromptSource{Kind: kind}
		if kind == work.SourceSnippet {
			source.Snippet = "check"
			source.Digest = "version1"
		}
		if err := log.Submit(work.Human, work.PromptPayload{Workspace: ws, Conversation: conversation, Source: source, Text: text}, nil); err != nil {
			t.Fatal(err)
		}
	}
	for _, c := range []string{"a", "b"} {
		send("one", c, work.SourceSnippet, "Run checks")
		send("one", c, work.SourceTyped, "Also check coverage")
		send("one", c, work.SourceTyped, "Always run lint")
		send("one", c, work.SourceTyped, "Summarize changes")
		send("one", c, work.SourceMission, "Automated brief")
	}
	send("two", "c", work.SourceTyped, "Summarize changes")
	got := workflowCandidates(log.Events(), "one")
	if len(got) != 3 {
		t.Fatal(got)
	}
	for _, key := range []string{"New snippet\nsummarize changes", "Repository instruction for AGENTS.md\nalways run lint", "Revision to snippet check\nalso check coverage\nversion1"} {
		c := got[key]
		if c == nil || len(c.conversations) != 2 || c.conversations["c"] {
			t.Fatalf("%s: %+v", key, c)
		}
	}
}
