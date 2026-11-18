package main

import (
	"bytes"
	"context"
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func TestReviewLinesAcceptARangeInTheFormsAgentsWrite(t *testing.T) {
	for text, want := range map[string][2]uint32{"12": {12, 12}, " 12-18 ": {12, 18}, "12:18": {12, 18}, "12..18": {12, 18}, "12–18": {12, 18}} {
		start, end, err := parseLines(text)
		if err != nil || start != want[0] || end != want[1] {
			t.Errorf("%q: %d-%d %v", text, start, end, err)
		}
	}
	for _, bad := range []string{"", "0", "x", "18-12", "1-", "-3"} {
		if _, _, err := parseLines(bad); exitCode(err) != exitUsage {
			t.Errorf("%q accepted: %v", bad, err)
		}
	}
}

// review add says what is missing before it dials anything, and refuses to
// run outside an agent conversation.
func TestReviewAddExplainsWhatIsMissing(t *testing.T) {
	var out, errs bytes.Buffer
	c := &cli{ctx: context.Background(), stdout: &out, stderr: &errs, getenv: func(string) string { return "" }}
	if err := c.review([]string{"add", "--file", "api/a.go", "--line", "3"}); exitCode(err) != exitUsage || !strings.Contains(err.Error(), "--severity and --title are required") {
		t.Fatalf("missing flags: %v", err)
	}
	err := c.review([]string{"add", "--file", "api/a.go", "--line", "3", "--severity", "nit", "--title", "x"})
	if exitCode(err) != exitUsage || !strings.Contains(err.Error(), "review conversation Lazybox started") {
		t.Fatalf("outside an agent: %v", err)
	}
	if err = c.review([]string{"bogus"}); exitCode(err) != exitUsage {
		t.Fatalf("unknown subcommand: %v", err)
	}
	// cancel needs the review or its workspace, before it dials anything.
	if err = c.review([]string{"cancel"}); exitCode(err) != exitUsage || !strings.Contains(err.Error(), "--workspace") {
		t.Fatalf("cancel without a review: %v", err)
	}
}

// The person's terminal cancels the one review under way in a workspace:
// the review reads cancelled, its reviewer's session ended; a second cancel
// finds nothing under way.
func TestReviewCancelFromTheTerminal(t *testing.T) {
	r := newTestRunner(t)
	author, _ := r.open("Fix the parser")
	started, err := r.service.StartReview(context.Background(), &pb.StartReviewRequest{CollectionId: r.collection.Id, SourceSessionId: author.Id, Scope: "changes", Provider: "codex"})
	if err != nil {
		t.Fatal(err)
	}
	reviewer := <-r.provider.opened
	out := r.cli(r.owner(), "", "review", "cancel", "--workspace", "Alpha feature")
	if out.err != nil || !strings.HasPrefix(out.stdout, "Review cancelled · 0 findings kept · Codex's conversation ended (Review · ") {
		t.Fatalf("cancel: %+v", out)
	}
	until(t, "the reviewer's session ended", reviewer.WasClosed)
	review, err := r.bench.Reviews.Load(r.collection.Id)
	if err != nil || review.Runs[0].ID != started.Run.Id || review.Runs[0].State != "cancelled" || review.Runs[0].StoppedBy != "You" {
		t.Fatalf("review: %+v %v", review.Runs, err)
	}
	if out = r.cli(r.owner(), "", "review", "cancel", "--workspace", "Alpha feature"); out.code != exitUsage || !strings.Contains(out.err.Error(), "no review is under way") {
		t.Fatalf("nothing under way: %+v", out)
	}
	// By its id, the Runner finds its workspace; cancelling it again is harmless.
	if out = r.cli(r.owner(), "", "review", "cancel", started.Run.Id, "--json"); out.err != nil || !strings.Contains(strings.Join(strings.Fields(out.stdout), " "), `"state": "cancelled"`) {
		t.Fatalf("by id: %+v", out)
	}
}
