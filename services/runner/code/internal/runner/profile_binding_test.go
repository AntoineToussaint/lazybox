package runner

import (
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
)

// Regression: renaming a subscription made every conversation on it refuse to
// resume ("saved account configuration changed"). A rename records the
// original name in EnrollmentLabel (accounts.RenameProfile), which the saved
// account binding used to include. A name is not which account runs.
func TestRenamingASubscriptionKeepsItsConversationsResumable(t *testing.T) {
	before := accounts.Profile{ID: "codex-work", Label: "Work", Provider: "codex", Home: "/state/accounts/codex-work", Isolation: "environment"}
	startedBefore := profileBinding(before)
	original := before.Label
	renamed := before
	renamed.Label, renamed.EnrollmentLabel = "Team subscription", &original
	if !sameAccount(renamed, startedBefore) {
		t.Fatal("a conversation started before the rename cannot resume after it")
	}
	// Started after the rename: the saved format is unchanged, so an earlier
	// Runner (after a rollback) reads it the same way.
	startedAfter := profileBinding(renamed)
	again := renamed
	again.Label = "Renamed twice"
	for name, binding := range map[string]string{"started before": startedBefore, "started after": startedAfter} {
		if !sameAccount(again, binding) {
			t.Errorf("%s: a renamed subscription refused its conversation", name)
		}
		moved := again
		moved.Home = "/state/accounts/another"
		if sameAccount(moved, binding) {
			t.Errorf("%s: another account was accepted as the conversation's", name)
		}
	}
	if sameAccount(renamed, "") {
		t.Fatal("a conversation with no saved binding was accepted")
	}
}
