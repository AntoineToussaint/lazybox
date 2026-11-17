package checkpoints

import (
	"strings"
	"testing"
)

func TestExplicitCheckpointPreservesEvidenceAndLimits(t *testing.T) {
	if _, err := (Checkpoint{Objective: "continue"}).Prompt(); err == nil {
		t.Fatal("incomplete handoff accepted")
	}
	c := Checkpoint{Objective: "implement feature", Criteria: []string{"tests pass"}, SourceRevisions: []string{"repo@revision"}, WorkingTreeState: "inspected: one modified file", CommandIDs: []string{"existing-command"}, OmittedContext: []string{"pending requests are not restored"}}
	prompt, err := c.Prompt()
	if err != nil || !strings.Contains(prompt, "existing-command") || !strings.Contains(prompt, "Do not repeat") {
		t.Fatalf("%s %v", prompt, err)
	}
	c.WorkingTreeState = strings.Repeat("x", 129<<10)
	if _, err = c.Prompt(); err == nil {
		t.Fatal("unbounded checkpoint accepted")
	}
}
