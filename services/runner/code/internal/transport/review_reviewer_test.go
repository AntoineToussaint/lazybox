package transport

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/availability"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// saveReviewer sets the default reviewer setting and returns what the Runner kept.
func saveReviewer(t *testing.T, f *reviewFixture, mode, reviewer, model string) *pb.ReviewSettings {
	t.Helper()
	settings, err := f.client.GetRunnerSettings(f.owner, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	settings.Review.ReviewerMode, settings.Review.Reviewer, settings.Review.ReviewerModel = mode, reviewer, model
	if settings, err = f.client.SaveRunnerSettings(f.owner, settings); err != nil {
		t.Fatal(err)
	}
	return settings.Review
}

// briefOf waits for the brief the reviewer's agent received.
func briefOf(t *testing.T, applied chan agent.Operation) agent.Operation {
	t.Helper()
	select {
	case op := <-applied:
		return op
	case <-timeout(t).Done():
		t.Fatal("the brief never reached the reviewer")
	}
	return agent.Operation{}
}

// The default reviewer is a setting of the Runner's: the other agent by
// default (an older setting without a mode keeps meaning what it meant),
// the author's own agent, or a fixed one with its model. A stale reviewer is
// not kept for a mode that names none, and nonsense is refused.
func TestReviewerSettingIsKept(t *testing.T) {
	f := newReviewFixture(t)
	settings, err := f.client.GetRunnerSettings(f.owner, &pb.InfoRequest{})
	if err != nil || settings.Review.ReviewerMode != "other" || settings.Review.Reviewer != "" {
		t.Fatalf("the default is the other agent: %+v %v", settings.GetReview(), err)
	}
	if kept := saveReviewer(t, f, "fixed", "codex-default", "gpt-5.2"); kept.ReviewerMode != "fixed" || kept.Reviewer != "codex-default" || kept.ReviewerModel != "gpt-5.2" {
		t.Fatalf("fixed: %+v", kept)
	}
	if kept := saveReviewer(t, f, "same", "codex-default", "gpt-5.2"); kept.ReviewerMode != "same" || kept.Reviewer != "" || kept.ReviewerModel != "" {
		t.Fatalf("same keeps no reviewer: %+v", kept)
	}
	// A client from before the mode: a reviewer named is a fixed one.
	if kept := saveReviewer(t, f, "", "codex-default", ""); kept.ReviewerMode != "fixed" || kept.Reviewer != "codex-default" {
		t.Fatalf("an older client's reviewer: %+v", kept)
	}
	for _, bad := range [][2]string{{"sometimes", ""}, {"fixed", ""}} {
		settings, _ := f.client.GetRunnerSettings(f.owner, &pb.InfoRequest{})
		settings.Review.ReviewerMode, settings.Review.Reviewer = bad[0], bad[1]
		if _, err := f.client.SaveRunnerSettings(f.owner, settings); err == nil {
			t.Errorf("%v accepted", bad)
		}
	}
}

// StartReview with no reviewer named follows the setting: the other agent
// (Codex reviews Claude's work), the same agent in a fresh conversation, or
// the fixed subscription on its model; the person's focus reaches the brief.
func TestStartReviewUsesTheConfiguredReviewer(t *testing.T) {
	f := newReviewFixture(t)
	request := &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Focus: "the error path of Handle", RequestKey: "review-request"}
	started, err := f.client.StartReview(f.owner, request)
	if err != nil {
		t.Fatal(err)
	}
	replay, err := f.client.StartReview(f.owner, request)
	if err != nil || replay.GetRun().GetId() != started.Run.Id || replay.GetSession().GetId() != started.Session.Id {
		t.Fatalf("replaying a review start created different work: %v %v", replay, err)
	}
	if started.Run.ReviewerProvider != "codex" || started.Run.Fallback != "" || started.Run.Focus != "the error path of Handle" || started.Session.Title != "Review · Changes · Codex" {
		t.Fatalf("the other agent reviews: %+v", started.Run)
	}
	if brief := briefOf(t, f.codex.Session.Applied); !strings.Contains(brief.Text, "## What the person asked you to focus on\n\n> the error path of Handle") {
		t.Fatalf("the focus is in the brief:\n%s", brief.Text)
	}

	if _, err = f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: started.Run.Id}); err != nil {
		t.Fatal(err)
	}
	saveReviewer(t, f, "same", "", "")
	same, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID})
	if err != nil {
		t.Fatal(err)
	}
	if same.Run.ReviewerProvider != "claude" || same.Session.Id == f.conversation.ID || same.Run.Fallback != "" || same.Session.Permission != "read-only" {
		t.Fatalf("the same agent, a fresh conversation: %+v %+v", same.Run, same.Session)
	}
	briefOf(t, f.claude.Session.Applied)

	if _, err = f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: same.Run.Id}); err != nil {
		t.Fatal(err)
	}
	saveReviewer(t, f, "fixed", "codex-default", "gpt-5.2-codex")
	fixed, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID})
	if err != nil {
		t.Fatal(err)
	}
	if fixed.Run.ReviewerProfileId != "codex-default" || f.codex.Session.Open.Model != "gpt-5.2-codex" || f.codex.Session.Open.Permission != "read-only" {
		t.Fatalf("the fixed reviewer on its model: %+v (opened with %+v)", fixed.Run, f.codex.Session.Open)
	}
	briefOf(t, f.codex.Session.Applied)
	if _, err = f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: fixed.Run.Id}); err != nil {
		t.Fatal(err)
	}
	// A reviewer named for one run (the chevron, `/review claude`) outranks the setting.
	named, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Provider: "claude"})
	if err != nil || named.Run.ReviewerProvider != "claude" {
		t.Fatalf("a named reviewer: %+v %v", named.GetRun(), err)
	}
	if _, err = f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: named.Run.Id}); err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Focus: strings.Repeat("x", 33<<10)}); err == nil {
		t.Fatal("a focus over 32 KiB was accepted")
	}
}

// The other agent is not installed: the author's own agent reviews, in a
// fresh conversation, and the review says so in its first line.
func TestStartReviewFallsBackWhenTheOtherAgentIsNotAvailable(t *testing.T) {
	f := newReviewFixture(t)
	worker := filepath.Join(t.TempDir(), "worker.mjs")
	if err := os.WriteFile(worker, []byte("//"), 0o600); err != nil {
		t.Fatal(err)
	}
	f.bench.Availability = &availability.Cache{Probe: availability.Probe{ClaudeBinary: "/bin/sh", Worker: worker, Node: "/bin/sh",
		LookPath: func(name string) (string, error) {
			if name == "codex" {
				return "", errors.New("not found")
			}
			return "/fixture/" + name, nil
		},
		Version: func(context.Context, string) (string, error) { return "1.0.0", nil },
	}}
	f.bench.Availability.Programs(timeout(t), false)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID})
	if err != nil {
		t.Fatal(err)
	}
	if started.Run.ReviewerProvider != "claude" || started.Session.Id == f.conversation.ID || started.Run.Fallback != "Codex is not available, so a fresh Claude conversation reviews." || !strings.HasSuffix(started.Run.Detail, started.Run.Fallback) {
		t.Fatalf("fallback: %+v", started.Run)
	}
	briefOf(t, f.claude.Session.Applied)
	if _, err = f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: started.Run.Id}); err != nil {
		t.Fatal(err)
	}
	// Asked for by name, an agent that is not available is refused, not replaced.
	if _, err = f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Provider: "codex"}); err == nil || !strings.Contains(err.Error(), "Codex is not available") {
		t.Fatalf("a named reviewer that is not available: %v", err)
	}
}

// Codex is installed but its only subscription needs a new sign-in: the
// same fallback.
func TestStartReviewFallsBackWhenTheOtherAgentIsSignedOut(t *testing.T) {
	f := newReviewFixture(t)
	if err := f.bench.Accounts.Observe("codex-default", agent.AccountObservation{State: "auth_required"}); err != nil {
		t.Fatal(err)
	}
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID})
	if err != nil {
		t.Fatal(err)
	}
	if started.Run.ReviewerProvider != "claude" || started.Run.Fallback != "Codex is not available, so a fresh Claude conversation reviews." {
		t.Fatalf("fallback: %+v", started.Run)
	}
}
