package transport

import (
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// A reviewer is a type of its own: not listed beside the work it reviews,
// its needs still in the inbox. Sending its findings with close_reviewers
// ends and archives it once nothing of its review is left: a partial send,
// or a send without the option, leaves it open. Its findings stay readable.
func TestReviewerTypeClosesOnceItsFindingsAreAllHandedOn(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	kind := started.Session.AgentType
	if started.Session.Purpose != agent.Reviewer || kind.GetName() != agent.Reviewer || kind.Listed || !kind.ReachesInbox || !kind.CloseWhenDelivered || kind.Delegates || kind.Ui != "reviewer" {
		t.Fatalf("a reviewer is typed as one: %q %+v", started.Session.Purpose, kind)
	}
	if author, err := f.client.GetSession(f.owner, &pb.SessionRequest{SessionId: f.conversation.ID}); err != nil || !author.Session.AgentType.Listed || author.Session.AgentType.Name != agent.Coding {
		t.Fatalf("the author stays a listed coding agent: %+v %v", author.GetSession().GetAgentType(), err)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	var ids []string
	for _, title := range []string{"Logged, not returned", "Retry loops forever"} {
		filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: started.Session.Id, File: "api/handler.go", StartLine: 3, Severity: "major", Title: title})
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, filed.Finding.Id)
	}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	deadline := time.Now().Add(5 * time.Second)
	for {
		tray, err := f.client.ListReviewComments(f.owner, &pb.ListReviewCommentsRequest{CollectionId: f.mine.Id})
		if err == nil && tray.Runs[0].State == "done" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("the review never finished: %v", err)
		}
		time.Sleep(20 * time.Millisecond)
	}
	if _, err = f.client.SetReviewDisposition(f.owner, &pb.SetReviewDispositionRequest{CollectionId: f.mine.Id, Ids: ids, Disposition: "open"}); err != nil {
		t.Fatal(err)
	}
	open := func() *pb.Session {
		t.Helper()
		got, err := f.client.GetSession(f.owner, &pb.SessionRequest{SessionId: started.Session.Id})
		if err != nil {
			t.Fatal(err)
		}
		return got.Session
	}
	send := func(id string, close bool) {
		t.Helper()
		if _, err := f.client.SetReviewDisposition(f.owner, &pb.SetReviewDispositionRequest{CollectionId: f.mine.Id, Ids: []string{id}, Disposition: "sent", SessionId: f.conversation.ID, SessionTitle: "Fix the parser", CloseReviewers: close}); err != nil {
			t.Fatal(err)
		}
	}
	// One finding of two sent: the other is still to hand on.
	send(ids[0], true)
	if s := open(); s.Closed || s.ArchivedAtUnixMillis != 0 {
		t.Fatalf("a reviewer with a finding left was closed: %+v", s)
	}
	// The last one, without the option: the person keeps it.
	send(ids[1], false)
	if s := open(); s.Closed || s.ArchivedAtUnixMillis != 0 {
		t.Fatalf("a send without close_reviewers closed the reviewer: %+v", s)
	}
	// Sent again with the option (a second window): nothing is left, so it ends.
	send(ids[1], true)
	if s := open(); !s.Closed || s.ArchivedAtUnixMillis == 0 {
		t.Fatalf("a delivered reviewer stays: closed %v archived %d", s.Closed, s.ArchivedAtUnixMillis)
	}
	tray, err := f.client.ListReviewComments(f.owner, &pb.ListReviewCommentsRequest{CollectionId: f.mine.Id})
	if err != nil || len(tray.Comments) != 2 || tray.Runs[0].ReviewerSessionId != started.Session.Id {
		t.Fatalf("its findings and review outlive it: %+v %v", tray, err)
	}
}
