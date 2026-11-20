package main

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// agentEnv is the environment Lazybox gives an agent in r's workspace.
func (r *testRunner) agentEnv(s *pb.Session, env []string) map[string]string {
	r.t.Helper()
	out := map[string]string{}
	for _, entry := range env {
		for _, name := range []string{"LAZYBOX_V2_ENDPOINT", "LAZYBOX_V2_SESSION_ID", "LAZYBOX_V2_COLLECTION_ID"} {
			if v, ok := strings.CutPrefix(entry, name+"="); ok {
				out[name] = v
			}
		}
	}
	if out["LAZYBOX_V2_ENDPOINT"] == "" || out["LAZYBOX_V2_SESSION_ID"] != s.Id || out["LAZYBOX_V2_COLLECTION_ID"] != r.collection.Id {
		r.t.Fatalf("agent environment incomplete: %v", out)
	}
	return out
}

// catalogRepository registers a git folder the workspace does not hold.
func (r *testRunner) catalogRepository(name string) *pb.Repository {
	r.t.Helper()
	path := filepath.Join(r.t.TempDir(), name)
	for _, args := range [][]string{{"init", "-b", "main", path}, {"-C", path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", args...).CombinedOutput(); err != nil {
			r.t.Fatal(err, string(out))
		}
	}
	added, err := r.service.AddLocalRepository(context.Background(), &pb.AddLocalRepositoryRequest{Path: path, Name: name})
	if err != nil {
		r.t.Fatal(err)
	}
	return added.Repository
}

func (r *testRunner) waitingRequests() []*pb.RepositoryRequest {
	r.t.Helper()
	list, err := r.service.ListWorkspaceRepositories(context.Background(), &pb.ListWorkspaceRepositoriesRequest{CollectionId: r.collection.Id})
	if err != nil {
		r.t.Fatal(err)
	}
	return list.Requests
}

func TestAgentAsksForARepositoryFromTheCLI(t *testing.T) {
	r := newTestRunner(t)
	session, native := r.open("parser")
	agent := r.agentEnv(session, native.Open.Env)
	module := r.catalogRepository("module-directory")
	tools := r.catalogRepository("tools")
	r.catalogRepository("third-party")

	// What the workspace holds, where, on which branch.
	out := r.cli(agent, "", "workspace", "repos")
	if out.code != 0 || !strings.Contains(out.stdout, "Alpha feature · branch ") || !strings.Contains(out.stdout, "api") || !strings.Contains(out.stdout, r.collection.Root) {
		t.Fatalf("repos: %+v", out)
	}
	// A reason is required: the person decides from it.
	if out = r.cli(agent, "", "workspace", "request-repo", "module-directory"); out.code != exitUsage || !strings.Contains(out.err.Error(), "--reason") {
		t.Fatalf("no reason: %+v", out)
	}
	// Asking without waiting returns at once; nothing is added.
	out = r.cli(agent, "", "workspace", "request-repo", "module_directory", "--reason", "the parser imports its tokenizer", "--no-wait")
	if out.code != 0 || !strings.Contains(out.stdout, "Asked the person to add module-directory to this workspace (branch ") || !strings.Contains(out.stdout, "Not waiting") {
		t.Fatalf("no-wait: %+v", out)
	}
	waiting := r.waitingRequests()
	if len(waiting) != 1 || waiting[0].Candidates[0].Identity != module.GitIdentity || waiting[0].Reason != "the parser imports its tokenizer" {
		t.Fatalf("%+v", waiting)
	}
	if out = r.cli(agent, "", "workspace", "repos"); !strings.Contains(out.stdout, "Waiting for the person to add:\n  module-directory · asked ") {
		t.Fatalf("repos with a request: %+v", out)
	}

	// Waiting: the person adds it, and the command prints the new worktree.
	done := make(chan result, 1)
	go func() {
		done <- r.cli(agent, "", "workspace", "request-repo", "module-directory", "--reason", "the parser imports its tokenizer", "--timeout", "15s")
	}()
	until(t, "the agent waits", func() bool { return strings.Contains(r.session(session.Id).String(), "repository_request") })
	time.Sleep(50 * time.Millisecond)
	added, err := r.bench.AddRequestedRepositories(context.Background(), "answer", r.collection.Id, waiting[0].Id, []string{module.GitIdentity}, nil, func(string, string) error { return nil })
	if err != nil {
		t.Fatal(err)
	}
	out = <-done
	if out.code != 0 || !strings.Contains(out.stdout, "The person added it to this workspace:") || !strings.Contains(out.stdout, added.Members[1].Path) || !strings.Contains(out.stdout, "(branch "+added.Members[1].Branch+")") {
		t.Fatalf("added: %+v", out)
	}
	// Asking for it again says where it already is.
	if out = r.cli(agent, "", "workspace", "request-repo", "module-directory", "--reason", "again"); out.code != 0 || !strings.Contains(out.stdout, "module-directory is already in this workspace:") {
		t.Fatalf("already: %+v", out)
	}

	// Declined, with the person's reason: exit 4.
	go func() {
		done <- r.cli(agent, "", "workspace", "request-repo", "tools", "--reason", "lint config", "--timeout", "15s")
	}()
	until(t, "the tools request", func() bool { return len(r.waitingRequests()) == 1 })
	if _, err = r.bench.DeclineRepositoryRequest(r.waitingRequests()[0].Id, "use the vendored copy"); err != nil {
		t.Fatal(err)
	}
	out = <-done
	if out.code != exitDenied || !strings.Contains(out.err.Error(), "declined adding tools. Their reason: use the vendored copy") || !strings.Contains(out.err.Error(), "Do not read or change it outside the workspace") {
		t.Fatalf("declined: %+v", out)
	}
	_ = tools

	// No answer in time: exit 5, and the request stays open.
	out = r.cli(agent, "", "workspace", "request-repo", "third-party", "--reason", "fixtures", "--timeout", "300ms")
	if out.code != exitTimeout || !strings.Contains(out.err.Error(), "no answer yet") || !strings.Contains(out.err.Error(), "you will be told in this conversation") {
		t.Fatalf("timeout: %+v", out)
	}
	left := r.waitingRequests()
	if len(left) != 1 || left[0].Candidates[0].Name != "third-party" {
		t.Fatalf("a timed-out request must stay open: %+v", left)
	}
	for _, want := range []string{left[0].Id, "wait ended", "lazybox workspace repos", "do not submit a duplicate request"} {
		if !strings.Contains(out.err.Error(), want) {
			t.Errorf("timeout lacks %q: %v", want, out.err)
		}
	}
	// From the person's own terminal: deny declines it; adding happens in the app.
	if out = r.cli(r.owner(), "", "approve", left[0].Id); out.code != exitUsage || !strings.Contains(out.err.Error(), "add it from its card in the app") {
		t.Fatalf("approve a repository request: %+v", out)
	}
	if out = r.cli(r.owner(), "", "deny", left[0].Id); out.code != 0 || !strings.Contains(out.stdout, "Declined in “parser”: Repository: Add third-party to this workspace") {
		t.Fatalf("deny a repository request: %+v", out)
	}
	if left := r.waitingRequests(); len(left) != 0 {
		t.Fatalf("declined request still waiting: %+v", left)
	}
	// JSON for scripts.
	if out = r.cli(agent, "", "workspace", "repos", "--json"); out.code != 0 || !strings.Contains(out.stdout, `"repositories"`) {
		t.Fatalf("json: %+v", out)
	}

	// Only agents ask: a person's own terminal adds through the window.
	if out = r.cli(r.owner(), "", "workspace", "request-repo", "tools", "--reason", "x"); out.code != exitUsage || !strings.Contains(out.err.Error(), "Workspace → Add repositories") {
		t.Fatalf("owner request: %+v", out)
	}
	// The owner names the workspace.
	if out = r.cli(r.owner(), "", "workspace", "repos", "Alpha"); out.code != 0 || !strings.Contains(out.stdout, "module-directory") {
		t.Fatalf("owner repos: %+v", out)
	}
}

func TestRequestRepoMakesPathsAbsolute(t *testing.T) {
	c := &cli{getenv: func(name string) string { return map[string]string{"HOME": "/home/me"}[name] }}
	cwd, _ := filepath.Abs(".")
	for in, want := range map[string]string{
		"~/src/module":       "/home/me/src/module",
		"./vendor/lib":       filepath.Join(cwd, "vendor/lib"),
		"../sibling":         filepath.Join(filepath.Dir(cwd), "sibling"),
		"/abs/path/":         "/abs/path",
		"acme/module":        "acme/module",
		"module-directory":   "module-directory",
		"https://github.com": "https://github.com",
	} {
		if got, err := c.repositoryQuery(in); err != nil || got != want {
			t.Errorf("%q: %q %v, want %q", in, got, err, want)
		}
	}
}

func TestWorkspaceDefinitionRoundTrip(t *testing.T) {
	r := newTestRunner(t)
	out := r.cli(r.owner(), "", "workspace", "export-definition", r.collection.Id)
	if out.code != 0 {
		t.Fatal(out.err)
	}
	path := filepath.Join(t.TempDir(), "collection.json")
	if err := os.WriteFile(path, []byte(out.stdout), 0600); err != nil {
		t.Fatal(err)
	}
	realize := func(key string) string {
		out := r.cli(r.owner(), "", "workspace", "realize", path, "--key", key, "--name", "From definition")
		if out.code != 0 {
			t.Fatal(out.err)
		}
		return strings.TrimSpace(out.stdout)
	}
	a := realize("first")
	if a == "" || a == r.collection.Id || realize("first") != a || realize("second") == a {
		t.Fatal("incorrect realization identity", a)
	}
}
