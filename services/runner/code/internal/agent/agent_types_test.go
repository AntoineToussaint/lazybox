package agent

import "testing"

// What each built-in type may do, where it is shown and how it ends. The
// gates that read these kept their answers for coding, artifact and the
// assistants; a reviewer is new.
func TestAgentTypes(t *testing.T) {
	for _, c := range []struct {
		purpose                                                                   string
		assistant, workspace, delegates, reviewable, listed, inbox, closeWhenDone bool
		ui                                                                        string
	}{
		{"", false, true, true, true, true, true, false, "chat"},
		{Coding, false, true, true, true, true, true, false, "chat"},
		{Reviewer, false, true, false, false, false, true, true, "reviewer"},
		{Artifact, false, true, false, true, false, true, false, "companion"},
		{Help, true, false, false, false, false, false, false, "assistant"},
		{Planning, true, false, false, false, false, false, false, "assistant"},
		{Summary, true, false, false, false, false, false, false, "assistant"},
	} {
		got := TypeOf(c.purpose)
		if got.Assistant != c.assistant || got.Workspace != c.workspace || got.Delegates != c.delegates || got.Reviewable != c.reviewable ||
			got.Listed != c.listed || got.ReachesInbox != c.inbox || got.CloseWhenDelivered != c.closeWhenDone || got.UI != c.ui || got.Label == "" {
			t.Errorf("%q: %+v", c.purpose, got)
		}
		if !ValidPurpose(c.purpose) || IsAssistant(c.purpose) != c.assistant {
			t.Errorf("%q: valid %v assistant %v", c.purpose, ValidPurpose(c.purpose), IsAssistant(c.purpose))
		}
	}
	// An unknown purpose is refused at open, and read as the narrowest type.
	if ValidPurpose("deploy") || IsAssistant("deploy") {
		t.Fatal("an unknown purpose is valid or an assistant")
	}
	if u := TypeOf("deploy"); u.Workspace || u.Delegates || u.Listed || u.ReachesInbox {
		t.Fatalf("an unknown purpose may act: %+v", u)
	}
}
