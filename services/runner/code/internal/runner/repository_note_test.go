package runner

import (
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
)

func reviewRun(round, rounds int) reviews.Run { return reviews.Run{Round: round, MaxRounds: rounds} }

// The answer to a repository request is two things: the words the agent is
// told, unchanged, and the event the conversation shows ("Added authority to
// this workspace", its branch beside it, its path to copy).
func TestRepositoryNoteSaysWhatTheAgentIsToldAndWhatTheConversationShows(t *testing.T) {
	added := RepositoryRequest{Query: "codefly-dev/authority", State: RequestAdded, Added: []WorkspaceRepository{{Name: "authority", Path: "/cache/collections/firefly/authority", Branch: "feature/revenge"}}}
	text, origin := repositoryNote(added)
	if !strings.HasPrefix(text, "[Lazybox] You asked to add codefly-dev/authority to this workspace. The person added it:\n- authority at /cache/collections/firefly/authority (branch feature/revenge)") {
		t.Fatalf("the agent's text changed: %q", text)
	}
	if origin == nil || origin.Kind != agent.OriginRepositoryAdded || origin.Summary != "Added authority to this workspace" || origin.Detail != "feature/revenge" || origin.Copy != "/cache/collections/firefly/authority" {
		t.Fatalf("added: %+v", origin)
	}
	declined := RepositoryRequest{Query: "billing", State: RequestDeclined, DeclineReason: "not this sprint"}
	text, origin = repositoryNote(declined)
	if !strings.Contains(text, "The person declined. Their reason: not this sprint") {
		t.Fatalf("the agent's text changed: %q", text)
	}
	if origin == nil || origin.Kind != agent.OriginRepositoryDeclined || origin.Summary != "Declined adding billing to this workspace" || origin.Detail != "not this sprint" {
		t.Fatalf("declined: %+v", origin)
	}
	if text, origin = repositoryNote(RepositoryRequest{State: RequestPending}); text != "" || origin != nil {
		t.Fatal("a pending request has nothing to say")
	}
}

// A review loop's messages say who they speak for and where the loop is.
func TestReviewLoopDetailNamesTheRoundOnlyForSeveralRounds(t *testing.T) {
	if got := joinDetail(roundLabel(reviewRun(2, 3)), "1 finding open"); got != "Round 2 of 3 · 1 finding open" {
		t.Fatal(got)
	}
	if got := joinDetail(roundLabel(reviewRun(1, 1)), "", "2 findings open"); got != "2 findings open" {
		t.Fatal(got)
	}
}
