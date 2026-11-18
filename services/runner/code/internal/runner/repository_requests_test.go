package runner

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

func (s *repositoryRequests) waitingFor(id string) int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.waiting[id]
}

func gitRepository(t *testing.T) string {
	t.Helper()
	path := t.TempDir()
	for _, args := range [][]string{{"init", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", append([]string{"-C", path}, args...)...).CombinedOutput(); err != nil {
			t.Fatal(err, string(out))
		}
	}
	return path
}

// requestFixture: a workspace holding "api", an agent working in it, and
// "module-directory" and "tools" in the catalog but not in the workspace.
type requestFixture struct {
	b         *Bench
	fake      *agenttest.Adapter
	workspace collections.Collection
	agent     *Execution
	module    string // module-directory's catalog identity
	tools     string
}

func newRequestFixture(t *testing.T) *requestFixture {
	t.Helper()
	t.Setenv("CLAUDE_CONFIG_DIR", t.TempDir())
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), WorktreeRoot: t.TempDir(), Repositories: []collections.Repository{
		{ID: "api", Path: gitRepository(t)}, {ID: "module", Name: "module-directory", Path: gitRepository(t)}, {ID: "tools", Name: "tools", Path: gitRepository(t)}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	b.repositoryRequests.grace = 20 * time.Millisecond
	fake := &agenttest.Adapter{}
	b.Adapters["claude"] = fake
	ws, err := b.Collections.Create(context.Background(), "requests", collections.Spec{Name: "Parser", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", Branch: "feature/parser"}}})
	if err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenCollection("claude-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	return &requestFixture{b: b, fake: fake, workspace: ws, agent: e, module: b.Collections.RepositoryIdentity("module"), tools: b.Collections.RepositoryIdentity("tools")}
}

func (f *requestFixture) ask(t *testing.T, query, reason string) RepositoryRequest {
	t.Helper()
	r, err := f.b.RequestRepository(context.Background(), f.agent.ID, query, reason)
	if err != nil {
		t.Fatal(err)
	}
	return r
}

func (f *requestFixture) add(t *testing.T, request string, identities ...string) collections.Collection {
	t.Helper()
	ws, err := f.b.AddRequestedRepositories(context.Background(), "answer-"+request, f.workspace.ID, request, identities, nil, func(string, string) error { return nil })
	if err != nil {
		t.Fatal(err)
	}
	return ws
}

// noMessage asserts nothing was sent to the agent within the note's grace.
func (f *requestFixture) noMessage(t *testing.T) {
	t.Helper()
	select {
	case op := <-f.fake.Session.Applied:
		t.Fatalf("the agent was sent %+v", op)
	case <-time.After(10 * f.b.repositoryRequests.grace):
	}
}

func (f *requestFixture) message(t *testing.T) string {
	t.Helper()
	select {
	case op := <-f.fake.Session.Applied:
		if op.Kind != agent.Message {
			t.Fatalf("unexpected operation %+v", op)
		}
		return op.Text
	case <-time.After(5 * time.Second):
		t.Fatal("the agent was not told")
	}
	return ""
}

func TestRepositoryRequestIsAddedWhileTheAgentWaits(t *testing.T) {
	f := newRequestFixture(t)
	r := f.ask(t, "module_directory", "the parser imports its tokenizer")
	top, ok := r.Match()
	if r.State != RequestPending || !ok || top.Identity != f.module || top.Name != "module-directory" || r.Reason != "the parser imports its tokenizer" {
		t.Fatalf("%+v", r)
	}
	if r.Branch != "feature/parser" || r.Workspace != f.workspace.ID || r.Conversation != conversationOf(f.agent) || !strings.HasPrefix(r.ID, "repo-") {
		t.Fatalf("request not scoped to the asking conversation and its workspace's branch: %+v", r)
	}
	// Asking again for the same repository is the same request.
	if again := f.ask(t, "module-directory", "again"); again.ID != r.ID {
		t.Fatal("a repeated request was recorded twice", again)
	}
	if pending := f.b.PendingRepositoryRequests(f.agent); len(pending) != 1 || pending[0].ID != r.ID {
		t.Fatalf("not waiting in the conversation: %+v", pending)
	}
	// Nothing was added by asking.
	if ws, _ := f.b.Collections.Get(f.workspace.ID); len(ws.Members) != 1 {
		t.Fatal("a request added a repository", ws.Members)
	}

	answered := make(chan RepositoryRequest, 1)
	go func() {
		got, err := f.b.WaitRepositoryRequest(context.Background(), r.ID, 20*time.Second)
		if err != nil {
			t.Error(err)
		}
		answered <- got
	}()
	for f.b.repositoryRequests.waitingFor(r.ID) == 0 {
		time.Sleep(5 * time.Millisecond)
	}
	ws := f.add(t, r.ID, f.module)
	got := <-answered
	if got.State != RequestAdded || len(got.Added) != 1 || got.Added[0].Name != "module-directory" || got.Added[0].Branch != "feature/parser" || !got.Told {
		t.Fatalf("the waiting agent did not get the new worktree: %+v", got)
	}
	if info, err := os.Stat(got.Added[0].Path); err != nil || !info.IsDir() || got.Added[0].Path != ws.Members[1].Path {
		t.Fatal("added worktree missing", got.Added[0].Path, err)
	}
	// The agent had its answer from the wait: no note on top of it.
	f.noMessage(t)
	if pending := f.b.PendingRepositoryRequests(f.agent); len(pending) != 0 {
		t.Fatal("answered request still waiting", pending)
	}
	// Adding again (a retried click) changes nothing.
	if again := f.add(t, r.ID, f.module); len(again.Members) != 2 {
		t.Fatal("a retry added another worktree", again.Members)
	}
	// Now it is in the workspace: asking for it says where it is.
	already := f.ask(t, "module-directory", "")
	if !already.Already || already.State != RequestAdded || already.ID != "" || already.Added[0].Path != got.Added[0].Path {
		t.Fatalf("%+v", already)
	}
	_, members, requests, err := f.b.WorkspaceRepositories(f.workspace.ID)
	if err != nil || len(members) != 2 || members[1].Identity != f.module || len(requests) != 0 {
		t.Fatal(members, requests, err)
	}
}

func TestDeclinedRequestIsSentToAnAgentThatNoLongerWaits(t *testing.T) {
	f := newRequestFixture(t)
	r := f.ask(t, "tools", "")
	if r, err := f.b.WaitRepositoryRequest(context.Background(), r.ID, 50*time.Millisecond); err != nil || r.State != RequestPending {
		t.Fatal("an unanswered wait must time out pending", r, err)
	}
	declined, err := f.b.DeclineRepositoryRequest(r.ID, "use the vendored copy instead")
	if err != nil || declined.State != RequestDeclined || declined.DeclineReason != "use the vendored copy instead" {
		t.Fatal(declined, err)
	}
	note := f.message(t)
	if !strings.Contains(note, "declined") || !strings.Contains(note, "use the vendored copy instead") || !strings.Contains(note, "tools") {
		t.Fatalf("note: %q", note)
	}
	// A later wait still reads the answer, and nothing is sent twice.
	if got, err := f.b.WaitRepositoryRequest(context.Background(), r.ID, time.Second); err != nil || got.State != RequestDeclined || !got.Told {
		t.Fatal(got, err)
	}
	f.noMessage(t)
	if _, err = f.b.DeclineRepositoryRequest(r.ID, ""); err == nil {
		t.Fatal("a request was answered twice")
	}
	if _, err = f.b.AddRequestedRepositories(context.Background(), "late", f.workspace.ID, r.ID, []string{f.tools}, nil, func(string, string) error { return nil }); err == nil {
		t.Fatal("a declined request was added")
	}
}

func TestAddedRequestIsSentToAnAgentThatNoLongerWaits(t *testing.T) {
	f := newRequestFixture(t)
	r := f.ask(t, "tools", "")
	// The person adds it from Workspace → Add repositories, not the card.
	if _, err := f.b.PrepareWorkspace(context.Background(), "dialog", f.workspace.ID, "", "", []string{f.tools}, func(string, string) error { return nil }); err != nil {
		t.Fatal(err)
	}
	got, err := f.b.GetRepositoryRequest(r.ID)
	if err != nil || got.State != RequestAdded || got.Added[0].Name != "tools" {
		t.Fatal(got, err)
	}
	note := f.message(t)
	if !strings.Contains(note, "added") || !strings.Contains(note, got.Added[0].Path) || !strings.Contains(note, "feature/parser") {
		t.Fatalf("note: %q", note)
	}
}

func TestChoosingAnotherRepositoryAnswersTheRequest(t *testing.T) {
	f := newRequestFixture(t)
	r := f.ask(t, "modul", "")
	f.add(t, r.ID, f.tools)
	got, err := f.b.GetRepositoryRequest(r.ID)
	if err != nil || got.State != RequestAdded || len(got.Added) != 1 || got.Added[0].Name != "tools" {
		t.Fatal(got, err)
	}
	// A request is answered only in its own workspace.
	other, err := f.b.Collections.Create(context.Background(), "other", collections.Spec{Name: "Other"})
	if err != nil {
		t.Fatal(err)
	}
	next := f.ask(t, "module-directory", "")
	if _, err = f.b.AddRequestedRepositories(context.Background(), "cross", other.ID, next.ID, []string{f.module}, nil, func(string, string) error { return nil }); !errors.Is(err, ErrInvalidRepositoryRequest) {
		t.Fatal("a request was answered in another workspace", err)
	}
}

func TestRepositoryRequestExpiresAndIsRateLimited(t *testing.T) {
	f := newRequestFixture(t)
	s := f.b.repositoryRequests
	r := f.ask(t, "tools", "")
	now := time.Now()
	s.now = func() time.Time { return now.Add(repositoryRequestTTL + time.Minute) }
	if got, err := f.b.GetRepositoryRequest(r.ID); err != nil || got.State != RequestExpired {
		t.Fatal(got, err)
	}
	if len(f.b.PendingRepositoryRequests(f.agent)) != 0 {
		t.Fatal("an expired request still waits")
	}
	if _, err := f.b.DeclineRepositoryRequest(r.ID, ""); err == nil {
		t.Fatal("an expired request was answered")
	}
	f.noMessage(t)
	s.now = time.Now

	// At most three waiting at once in a conversation…
	var asked []RepositoryRequest
	for i := range 3 {
		asked = append(asked, f.ask(t, "unknown-"+string(rune('a'+i)), ""))
	}
	if _, err := f.b.RequestRepository(context.Background(), f.agent.ID, "unknown-z", ""); !errors.Is(err, ErrRepositoryRequestLimit) {
		t.Fatal("a fourth waiting request was accepted", err)
	}
	// …and ten an hour.
	for _, r := range asked {
		if _, err := f.b.DeclineRepositoryRequest(r.ID, ""); err != nil {
			t.Fatal(err)
		}
	}
	for i := range 6 {
		r := f.ask(t, "more-"+string(rune('a'+i)), "")
		if _, err := f.b.DeclineRepositoryRequest(r.ID, ""); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := f.b.RequestRepository(context.Background(), f.agent.ID, "one-too-many", ""); !errors.Is(err, ErrRepositoryRequestLimit) {
		t.Fatal("an eleventh request in an hour was accepted", err)
	}
}

func TestRepositoryRequestValidationAndPersistence(t *testing.T) {
	f := newRequestFixture(t)
	for _, bad := range []struct{ query, reason string }{{"", ""}, {"two\nlines", ""}, {strings.Repeat("x", 600), ""}, {"tools", "bell\a"}, {"tools", strings.Repeat("y", 1200)}} {
		if _, err := f.b.RequestRepository(context.Background(), f.agent.ID, bad.query, bad.reason); !errors.Is(err, ErrInvalidRepositoryRequest) {
			t.Errorf("%q/%q accepted: %v", bad.query, bad.reason, err)
		}
	}
	if _, err := f.b.RequestRepository(context.Background(), "missing", "tools", ""); err == nil {
		t.Fatal("an unknown execution asked")
	}
	// A git folder the catalog does not know is offered as itself.
	folder := gitRepository(t)
	inside := filepath.Join(folder, "pkg")
	if err := os.MkdirAll(inside, 0o755); err != nil {
		t.Fatal(err)
	}
	r := f.ask(t, inside, "vendored fork")
	if top, ok := r.Match(); !ok || top.Source != "folder" || top.LocalPath != folder || top.Identity != "" {
		t.Fatalf("%+v", r.Candidates)
	}
	// Requests outlive the Runner.
	reopened, err := openRepositoryRequests(f.b.repositoryRequests.path)
	if err != nil {
		t.Fatal(err)
	}
	if got := reopened.items[r.ID]; got == nil || got.State != RequestPending || got.Reason != "vendored fork" || got.Conversation != conversationOf(f.agent) {
		t.Fatalf("%+v", got)
	}
}
