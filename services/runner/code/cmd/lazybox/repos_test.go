package main

import (
	"encoding/json"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// The person and their agents both learn where a repository's home checkout
// is: the person sees the library with each home's branch and state, an agent
// its own workspace's homes; `repo home` prints one path for scripts.
func TestTheCLIListsHomeCheckouts(t *testing.T) {
	r := newTestRunner(t)
	repo, _ := r.bench.Collections.Repository("api")
	// The test repository is registered from a regular checkout: its own home.
	out := r.cli(r.owner(), "", "repos")
	if out.code != 0 || !strings.Contains(out.stdout, "api") || !strings.Contains(out.stdout, repo.Path) || !strings.Contains(out.stdout, "main · clean") || !strings.Contains(out.stdout, "read-only for agents") {
		t.Fatalf("owner repos: %+v", out)
	}
	if out = r.cli(r.owner(), "", "repo", "home", "api"); out.code != 0 || out.stdout != repo.Path+"\n" {
		t.Fatalf("repo home: %+v", out)
	}
	if out = r.cli(r.owner(), "", "repo", "home", "nothing"); out.code != exitNotFound {
		t.Fatalf("unknown repository: %+v", out)
	}

	// A home created elsewhere replaces it, for the person and the agent.
	mine := filepath.Join(t.TempDir(), "my-api")
	if b, err := exec.Command("git", "clone", "--quiet", repo.Path, mine).CombinedOutput(); err != nil {
		t.Fatal(err, string(b))
	}
	if out = r.cli(r.owner(), "", "repo", "home", "api", "--adopt", mine); out.code != 0 {
		t.Fatalf("adopt: %+v %v", out, out.err)
	}
	resolved, _ := filepath.EvalSymlinks(mine)
	if out = r.cli(r.owner(), "", "repo", "home", "api", "--json"); out.code != 0 {
		t.Fatalf("json: %+v", out)
	}
	var row homeRow
	if err := json.Unmarshal([]byte(out.stdout), &row); err != nil || row.Path != resolved || row.Kind != "configured" || row.Branch != "main" {
		t.Fatalf("json row: %+v %v", row, err)
	}

	// Update to latest main: fetched; nothing new, so nothing moved.
	if out = r.cli(r.owner(), "", "repo", "home", "api", "--update"); out.code != 0 || out.stdout != resolved+"\n" || !strings.Contains(out.stderr, "Already at the latest origin/main") {
		t.Fatalf("update: %+v %v", out, out.err)
	}

	session, native := r.open("login")
	agent := r.agentEnv(session, native.Open.Env)
	if out = r.cli(agent, "", "repos"); out.code != 0 || !strings.Contains(out.stdout, "api") || !strings.Contains(out.stdout, resolved) {
		t.Fatalf("an agent lists its workspace's homes: %+v", out)
	}
	if out = r.cli(agent, "", "repo", "home", "api"); out.code != 0 || out.stdout != resolved+"\n" {
		t.Fatalf("agent repo home: %+v", out)
	}
	if out = r.cli(agent, "", "workspace", "repos"); out.code != 0 || !strings.Contains(out.stdout, "Home checkouts (the person's own clones; read-only for agents):") || !strings.Contains(out.stdout, resolved) {
		t.Fatalf("workspace repos names the home: %+v", out)
	}
	// An agent can read where the home is, never change it.
	if out = r.cli(agent, "", "repo", "home", "api", "--forget"); out.code == 0 {
		t.Fatalf("an agent forgot the person's home: %+v", out)
	}
	if out = r.cli(r.owner(), "", "repo", "home", "api", "--forget"); out.code != 0 {
		t.Fatalf("forget: %+v", out)
	}
	if out = r.cli(r.owner(), "", "repo", "home", "api"); out.stdout != repo.Path+"\n" {
		t.Fatalf("after forgetting, the registered checkout is the home again: %+v", out)
	}
}
