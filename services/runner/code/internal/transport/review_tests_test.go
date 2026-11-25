package transport

import (
	"context"
	"encoding/json"
	"io/fs"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// checkoutState is every file of a checkout outside .git, byte for byte,
// with what Git says of it.
func checkoutState(t *testing.T, dir string) map[string]string {
	t.Helper()
	state := map[string]string{}
	err := filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.Name() == ".git" {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		rel, _ := filepath.Rel(dir, path)
		if d.Type()&fs.ModeSymlink != 0 {
			target, err := os.Readlink(path)
			state[rel] = "link:" + target
			return err
		}
		if d.IsDir() {
			state[rel+"/"] = ""
			return nil
		}
		data, err := os.ReadFile(path)
		state[rel] = string(data)
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{{"rev-parse", "HEAD"}, {"status", "--porcelain=v1", "--untracked-files=all", "--ignored"}, {"stash", "list"}} {
		out, err := exec.Command("git", append([]string{"-C", dir}, args...)...).CombinedOutput()
		if err != nil {
			t.Fatalf("git %v: %v: %s", args, err, out)
		}
		state["git "+strings.Join(args, " ")] = string(out)
	}
	return state
}

// originalCheckout is the checkout the workspace's worktree was made from,
// if it has one apart from the worktree.
func originalCheckout(t *testing.T, worktree string) string {
	t.Helper()
	out, err := exec.Command("git", "-C", worktree, "rev-parse", "--path-format=absolute", "--git-common-dir").Output()
	if err != nil {
		t.Fatal(err)
	}
	common := strings.TrimSpace(string(out))
	if filepath.Base(common) != ".git" {
		return "" // a bare repository: there is no other checkout
	}
	main := filepath.Dir(common)
	if same, _ := filepath.EvalSymlinks(main); same == mustEval(t, worktree) {
		return ""
	}
	return main
}

func mustEval(t *testing.T, path string) string {
	t.Helper()
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return resolved
}

// reviewRun is the review as the person and the author see it.
func reviewRun(t *testing.T, f *reviewFixture, id string) *pb.ReviewRun {
	t.Helper()
	review, err := f.client.ListReviewComments(f.owner, &pb.ListReviewCommentsRequest{CollectionId: f.mine.Id})
	if err != nil {
		t.Fatal(err)
	}
	for _, run := range review.Runs {
		if run.Id == id {
			return run
		}
	}
	t.Fatalf("review %s not found", id)
	return nil
}

// An ordinary review's reviewer has a read-only shell, where a test that
// writes anything fails. Its test runs in a disposable, writable checkout of
// the reviewed files instead: the test writes its caches and build output
// there, the author's worktree and the checkout it came from stay byte for
// byte as they were, and the result is recorded in the review. Nobody else,
// and no finished review, can run one.
func TestReviewerTestRunsInADisposableCheckout(t *testing.T) {
	f := newReviewFixture(t)
	worktree := f.mine.Members[0].Path
	before := checkoutState(t, worktree)
	original := originalCheckout(t, worktree)
	var originalBefore map[string]string
	if original != "" {
		originalBefore = checkoutState(t, original)
	}

	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID})
	if err != nil {
		t.Fatal(err)
	}
	brief := <-f.codex.Session.Applied
	if started.Run.TemplateId != "" || started.Session.Permission != "read-only" || !strings.Contains(brief.Text, "lazybox review test --member") {
		t.Fatalf("run %+v; brief %s", started.Run, brief.Text)
	}

	// The test sees the author's uncommitted change, writes a gate receipt,
	// a dependency cache, build output and a temporary file, then passes.
	script := `grep -q 'log.Print' handler.go && mkdir -p .cache/gates node_modules/.cache && printf receipt > .cache/gates/ui.json && printf x > node_modules/.cache/v && printf built > build.out && printf t > "$TMPDIR/scratch" && test "$(cat .cache/gates/ui.json)" = receipt`
	args, _ := json.Marshal(map[string]any{"action": "test", "member": "api", "argv": []string{"sh", "-c", script}})
	call := f.codex.Session.Open.Tools
	raw, err := call(timeout(t), "lazybox_command", args)
	if err != nil {
		t.Fatal(err)
	}
	var check reviews.VerificationCheck
	if err := json.Unmarshal([]byte(raw), &check); err != nil {
		t.Fatal(err)
	}
	if !check.Test() || len(check.Runs) != 1 || check.Runs[0].CandidateCommand == "" || check.Runs[0].BaseCommand != "" || check.Runs[0].Candidate == "" {
		t.Fatalf("test not started on the candidate alone: %s", raw)
	}
	receipt := check.Runs[0]
	waited, _ := json.Marshal(map[string]string{"action": "wait", "id": receipt.CandidateCommand})
	if _, err := call(timeout(t), "lazybox_command", waited); err != nil {
		t.Fatal(err)
	}
	command, err := f.bench.Commands.Get(receipt.CandidateCommand)
	if err != nil {
		t.Fatal(err)
	}
	if s := command.Status(); !s.Spec.Isolated || s.ExitCode == nil || *s.ExitCode != 0 || strings.HasPrefix(s.Spec.Cwd, mustEval(t, worktree)) {
		t.Fatalf("test did not pass in an isolated checkout: %+v: %s", s, command.Tail(context.Background(), 10000))
	}

	// Recorded where the person and the author see the review.
	run := reviewRun(t, f, started.Run.Id)
	if len(run.Verification) != 1 || run.Verification[0].Mode != reviews.ModeTest || len(run.Verification[0].Runs) != 1 ||
		run.Verification[0].Runs[0].State != "passed" || run.Verification[0].Runs[0].GetCandidateExit() != 0 || run.Verification[0].Runs[0].CandidateCommandId != receipt.CandidateCommand {
		t.Fatalf("test result not recorded: %+v", run.Verification)
	}

	// Only the reviewer: the author, with its own credential and session,
	// is not reviewing anything; with the reviewer's session it is refused.
	author := bearer(timeout(t), f.agentToken)
	if _, err := f.client.RunReviewVerification(author, &pb.RunReviewVerificationRequest{SessionId: f.conversation.ID, Mode: reviews.ModeTest, Member: "api", Argv: []string{"true"}}); err == nil {
		t.Fatal("the author ran a reviewer test")
	}
	if _, err := f.client.RunReviewVerification(author, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Mode: reviews.ModeTest, Member: "api", Argv: []string{"true"}}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("the author ran a test as the reviewer: %v", err)
	}

	// The review ends, and the author's conversation hears how its tests
	// went with the review's outcome: no test after it.
	f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Checked the parser; its tests pass."}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	if finished := events(t, f.bench, f.conversation.ID, "review_finished")["review_finished"]; !strings.Contains(finished.Text, "1 test: 1 passed") {
		t.Fatalf("the review's outcome lacks its tests: %q", finished.Text)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	if _, err := f.client.RunReviewVerification(reviewer, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Mode: reviews.ModeTest, Name: "after", Member: "api", Argv: []string{"true"}}); err == nil {
		t.Fatal("a finished review ran a test")
	}

	if after := checkoutState(t, worktree); !maps.Equal(before, after) {
		t.Fatalf("the worktree changed:\nbefore %q\nafter  %q", before, after)
	}
	if original != "" {
		if after := checkoutState(t, original); !maps.Equal(originalBefore, after) {
			t.Fatalf("the original checkout changed:\nbefore %q\nafter  %q", originalBefore, after)
		}
	}
}

// A review's tests are bounded: two run at once, a third waits its turn.
// They end with the review.
func TestReviewerTestsAreBoundedAndEndWithTheReview(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: reviews.CodeReviewTemplate})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	review, err := f.bench.Reviews.Load(f.mine.Id)
	if err != nil {
		t.Fatal(err)
	}
	var captured string
	for _, run := range review.Runs {
		if run.ID == started.Run.Id && len(run.Input) == 1 {
			captured = run.Input[0].Commit
		}
	}
	if captured == "" {
		t.Fatal("the code review captured no input")
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	var ids []string
	for _, name := range []string{"one", "two"} {
		check, err := f.client.RunReviewVerification(reviewer, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Mode: reviews.ModeTest, Name: name, Member: "api", Argv: []string{"sleep", "30"}})
		if err != nil {
			t.Fatal(err)
		}
		if check.Runs[0].Base != "" || check.Runs[0].Candidate != captured {
			t.Fatalf("a captured review's test did not run its captured candidate: %+v", check.Runs[0])
		}
		again, err := f.client.RunReviewVerification(reviewer, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Mode: reviews.ModeTest, Name: name, Member: "api", Argv: []string{"sleep", "30"}})
		if err != nil || again.Runs[0].CandidateCommandId != check.Runs[0].CandidateCommandId {
			t.Fatalf("a retry started another test: %+v %v", again, err)
		}
		ids = append(ids, check.Runs[0].CandidateCommandId)
	}
	if _, err := f.client.RunReviewVerification(reviewer, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Mode: reviews.ModeTest, Name: "three", Member: "api", Argv: []string{"true"}}); err == nil || !strings.Contains(err.Error(), "already running") {
		t.Fatalf("a third concurrent test started: %v", err)
	}
	if _, _, err := f.bench.CancelReview(timeout(t), f.mine.Id, started.Run.Id, "You"); err != nil {
		t.Fatal(err)
	}
	for _, id := range ids {
		command, err := f.bench.Commands.Get(id)
		if err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		state, err := command.Wait(ctx)
		cancel()
		if err != nil || !state.Done || (state.ExitCode != nil && *state.ExitCode == 0) {
			t.Fatalf("test outlived its review: %+v %v", state, err)
		}
	}
}
