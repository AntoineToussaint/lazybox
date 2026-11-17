package transport

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func commentByID(t *testing.T, review *pb.WorkspaceReview, id string) *pb.ReviewComment {
	t.Helper()
	for _, c := range review.Comments {
		if c.Id == id {
			return c
		}
	}
	t.Fatalf("comment %s missing from %v", id, review)
	return nil
}

// A comment through the Runner's RPCs: anchored to the lines it quotes,
// following them when they move, outdated (with its snippet) when they
// change, kept across a Runner restart, and resolved on its own only when the
// person asked and a commit after the handover changed those lines.
func TestReviewCommentsAnchorPersistAndResolveThroughTheRunner(t *testing.T) {
	t.Setenv("GIT_CONFIG_GLOBAL", os.DevNull)
	t.Setenv("GIT_CONFIG_NOSYSTEM", "1")
	s, _ := collectionService(t)
	ctx := context.Background()
	w, err := s.Bench.Collections.Create(ctx, "review", collections.Spec{Name: "Review", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	worktree := w.Members[0].Path
	runGit(t, worktree, "config", "user.name", "Fixture")
	runGit(t, worktree, "config", "user.email", "fixture@example.invalid")
	write := func(name, text string) {
		t.Helper()
		if err := os.WriteFile(filepath.Join(worktree, name), []byte(text), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	commit := func(message string) string {
		t.Helper()
		runGit(t, worktree, "add", "--all")
		runGit(t, worktree, "commit", "-q", "-m", message)
		return runGit(t, worktree, "rev-parse", "HEAD")
	}
	write("handler.go", "package api\n\nfunc Handle() error {\n\treturn nil\n}\n")
	first := commit("Add the handler")
	list := func() *pb.WorkspaceReview {
		t.Helper()
		review, err := s.ListReviewComments(ctx, &pb.ListReviewCommentsRequest{CollectionId: w.ID})
		if err != nil {
			t.Fatal(err)
		}
		return review
	}
	if review := list(); len(review.Comments) != 0 || review.AutoResolve {
		t.Fatalf("fresh workspace: %v", review)
	}

	added, err := s.AddReviewComment(ctx, &pb.AddReviewCommentRequest{CollectionId: w.ID, Member: "api", Path: "handler.go", Side: "new", StartLine: 3,
		Snippet: []string{"func Handle() error {", "\treturn nil"}, Body: "Say **why** this cannot fail.", View: "branch", Base: "main"})
	if err != nil || added.AddedId == "" {
		t.Fatal(added, err)
	}
	c := commentByID(t, added, added.AddedId)
	if c.Head != first || c.EndLine != 4 || c.Freshness != "current" || c.CurrentStartLine != 3 || c.Disposition != "open" || c.Base != "main" || c.Hash == "" {
		t.Fatalf("added: %+v", c)
	}
	removed, err := s.AddReviewComment(ctx, &pb.AddReviewCommentRequest{CollectionId: w.ID, Member: "api", Path: "handler.go", Side: "old", StartLine: 2, Snippet: []string{"// Deprecated"}, Body: "Keep the notice", View: "uncommitted"})
	if err != nil || commentByID(t, removed, removed.AddedId).Freshness != "current" {
		t.Fatal(removed, err)
	}
	for name, bad := range map[string]*pb.AddReviewCommentRequest{
		"another workspace's path": {CollectionId: w.ID, Member: "api", Path: "../etc/passwd", Side: "new", StartLine: 1, Snippet: []string{"x"}, Body: "x", View: "uncommitted"},
		"an unknown member":        {CollectionId: w.ID, Member: "web", Path: "a.go", Side: "new", StartLine: 1, Snippet: []string{"x"}, Body: "x", View: "uncommitted"},
		"no body":                  {CollectionId: w.ID, Member: "api", Path: "handler.go", Side: "new", StartLine: 1, Snippet: []string{"x"}, View: "uncommitted"},
	} {
		if _, err = s.AddReviewComment(ctx, bad); err == nil {
			t.Errorf("%s accepted", name)
		}
	}
	if _, err = s.ListReviewComments(ctx, &pb.ListReviewCommentsRequest{CollectionId: "missing"}); status.Code(err) != codes.NotFound {
		t.Fatalf("unknown workspace: %v", err)
	}

	// The lines move: the comment follows them. The removed-lines comment is
	// outdated because its file changed.
	write("handler.go", "package api\n\nimport \"errors\"\n\nfunc Handle() error {\n\treturn nil\n}\n")
	review := list()
	if c = commentByID(t, review, added.AddedId); c.Freshness != "moved" || c.CurrentStartLine != 5 || c.CurrentEndLine != 6 || c.StartLine != 3 {
		t.Fatalf("moved: %+v", c)
	}
	if old := commentByID(t, review, removed.AddedId); old.Freshness != "outdated" || !strings.Contains(old.FreshnessReason, "removed lines") {
		t.Fatalf("removed lines after the file changed: %+v", old)
	}
	// The lines change: outdated, and the original snippet still reads.
	write("handler.go", "package api\n\nfunc Handle() error {\n\treturn errors.New(\"no\")\n}\n")
	if c = commentByID(t, list(), added.AddedId); c.Freshness != "outdated" || c.CurrentStartLine != 0 || c.Snippet[1] != "\treturn nil" {
		t.Fatalf("outdated: %+v", c)
	}
	write("handler.go", "package api\n\nfunc Handle() error {\n\treturn nil\n}\n")

	// Edit and delete; then a Runner restart keeps what is left.
	if review, err = s.EditReviewComment(ctx, &pb.EditReviewCommentRequest{CollectionId: w.ID, Id: added.AddedId, Body: "Explain why it cannot fail."}); err != nil || commentByID(t, review, added.AddedId).Body != "Explain why it cannot fail." {
		t.Fatal(review, err)
	}
	if review, err = s.DeleteReviewComment(ctx, &pb.DeleteReviewCommentRequest{CollectionId: w.ID, Id: removed.AddedId}); err != nil || len(review.Comments) != 1 {
		t.Fatal(review, err)
	}
	if _, err = s.DeleteReviewComment(ctx, &pb.DeleteReviewCommentRequest{CollectionId: w.ID, Id: removed.AddedId}); status.Code(err) != codes.NotFound {
		t.Fatalf("deleting a deleted comment: %v", err)
	}
	if s.Bench.Reviews, err = reviews.New(filepath.Join(s.Bench.Config.State, "reviews")); err != nil {
		t.Fatal(err)
	}
	if review = list(); len(review.Comments) != 1 || review.Comments[0].Body != "Explain why it cannot fail." {
		t.Fatalf("after restart: %v", review)
	}

	// Sent to a conversation; an unrelated commit resolves nothing even with
	// auto-resolve on.
	if review, err = s.SetReviewDisposition(ctx, &pb.SetReviewDispositionRequest{CollectionId: w.ID, Ids: []string{added.AddedId}, Disposition: "sent", SessionId: "s1", SessionTitle: "Fix the handler"}); err != nil {
		t.Fatal(err)
	}
	if c = commentByID(t, review, added.AddedId); c.Disposition != "sent" || c.SentTo != "s1" || c.SentToTitle != "Fix the handler" || c.SentHead != first || c.SentAtUnix == 0 {
		t.Fatalf("sent: %+v", c)
	}
	if review, err = s.SetReviewAutoResolve(ctx, &pb.SetReviewAutoResolveRequest{CollectionId: w.ID, Enabled: true}); err != nil || !review.AutoResolve {
		t.Fatal(review, err)
	}
	write("README.md", "# API\n")
	commit("Add a readme")
	if c = commentByID(t, list(), added.AddedId); c.Disposition != "sent" {
		t.Fatalf("an unrelated commit resolved the comment: %+v", c)
	}
	// Changing the lines without committing resolves nothing either.
	write("handler.go", "package api\n\nfunc Handle() error {\n\treturn handle()\n}\n")
	if c = commentByID(t, list(), added.AddedId); c.Disposition != "sent" || c.Freshness != "outdated" {
		t.Fatalf("an uncommitted edit resolved the comment: %+v", c)
	}
	// The agent commits the change: resolved, shown with the commit.
	fix := commit("Handle the error")
	if c = commentByID(t, list(), added.AddedId); c.Disposition != "resolved" || c.ResolvedBy != fix || !strings.Contains(c.Note, fix[:8]) {
		t.Fatalf("committed change: %+v", c)
	}
	// Reopened by the person: it stays open.
	if review, err = s.SetReviewDisposition(ctx, &pb.SetReviewDispositionRequest{CollectionId: w.ID, Ids: []string{added.AddedId}, Disposition: "open"}); err != nil {
		t.Fatal(err)
	}
	if c = commentByID(t, list(), added.AddedId); c.Disposition != "open" || c.ResolvedBy != "" {
		t.Fatalf("reopened: %+v", c)
	}

	// Deleting the workspace forgets its review.
	if _, err = s.DeleteWorkspace(ctx, &pb.DeleteWorkspaceRequest{CollectionId: w.ID, ConfirmName: "Review", DiscardChanges: true, StopSessions: true}); err != nil {
		t.Fatal(err)
	}
	if left, _ := s.Bench.Reviews.Load(w.ID); len(left.Comments) != 0 {
		t.Fatal("a deleted workspace kept its review comments")
	}
}

// Agents run inside the workspace and hold a scoped credential; a review is
// the person's, so none of its RPCs is theirs, reading included.
func TestAgentCredentialCannotReadOrWriteReviewComments(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	_, _, token := f.open(t)
	agent := bearer(ctx, token)
	calls := map[string]error{}
	_, calls["ListReviewComments"] = f.client.ListReviewComments(agent, &pb.ListReviewCommentsRequest{CollectionId: "w"})
	_, calls["AddReviewComment"] = f.client.AddReviewComment(agent, &pb.AddReviewCommentRequest{CollectionId: "w", Member: "api", Path: "a.go", Side: "new", StartLine: 1, Snippet: []string{"x"}, Body: "x", View: "uncommitted"})
	_, calls["EditReviewComment"] = f.client.EditReviewComment(agent, &pb.EditReviewCommentRequest{CollectionId: "w", Id: "rc-1", Body: "x"})
	_, calls["DeleteReviewComment"] = f.client.DeleteReviewComment(agent, &pb.DeleteReviewCommentRequest{CollectionId: "w", Id: "rc-1"})
	_, calls["SetReviewDisposition"] = f.client.SetReviewDisposition(agent, &pb.SetReviewDispositionRequest{CollectionId: "w", Ids: []string{"rc-1"}, Disposition: "resolved"})
	_, calls["SetReviewAutoResolve"] = f.client.SetReviewAutoResolve(agent, &pb.SetReviewAutoResolveRequest{CollectionId: "w", Enabled: true})
	for name, err := range calls {
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: agent credential got %v, want PermissionDenied", name, err)
		}
	}
	if _, err := f.client.ListReviewComments(ctx, &pb.ListReviewCommentsRequest{CollectionId: "w"}); status.Code(err) != codes.Unauthenticated {
		t.Errorf("no credential got %v", err)
	}
}
