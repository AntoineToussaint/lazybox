package main

import (
	"context"
	"encoding/json"
	"os/exec"
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func gitFixture(t *testing.T, dir string, args ...string) string {
	t.Helper()
	out, err := exec.Command("git", append([]string{"-C", dir, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"}, args...)...).CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// `lazybox new --start-from B`: every repository starts from origin's B,
// fetched first (here origin moved on after the person's clone), and B is
// remembered for the repository's next workspace. A branch origin does not
// have fails with what to do.
func TestNewStartsFromAChosenBranch(t *testing.T) {
	r := newTestRunner(t)
	upstream := t.TempDir()
	gitFixture(t, upstream, "init", "-b", "main")
	gitFixture(t, upstream, "commit", "--allow-empty", "-m", "initial")
	gitFixture(t, upstream, "switch", "-c", "feat/v2")
	gitFixture(t, upstream, "commit", "--allow-empty", "-m", "v2")
	gitFixture(t, upstream, "switch", "main")
	checkout := t.TempDir()
	if out, err := exec.Command("git", "clone", "--quiet", upstream, checkout).CombinedOutput(); err != nil {
		t.Fatal(err, string(out))
	}
	gitFixture(t, upstream, "switch", "feat/v2")
	gitFixture(t, upstream, "commit", "--allow-empty", "-m", "v2 moves on")
	fresh := gitFixture(t, upstream, "rev-parse", "HEAD")
	gitFixture(t, upstream, "switch", "main")
	added, err := r.service.AddLocalRepository(context.Background(), &pb.AddLocalRepositoryRequest{Path: checkout})
	if err != nil {
		t.Fatal(err)
	}

	out := r.cli(r.owner(), "", "new", "Alpha", "--repo", added.Repository.GitIdentity, "--start-from", "feat/v2", "--json")
	if out.code != 0 {
		t.Fatalf("new --start-from: %+v %v", out, out.err)
	}
	var created struct{ ID string }
	if err := json.Unmarshal([]byte(out.stdout), &created); err != nil {
		t.Fatal(out.stdout, err)
	}
	got, err := r.service.GetCollection(context.Background(), &pb.CollectionRequest{CollectionId: created.ID})
	if err != nil || len(got.Collection.Members) != 1 {
		t.Fatal(got, err)
	}
	if head := gitFixture(t, got.Collection.Members[0].Path, "rev-parse", "HEAD"); head != fresh {
		t.Fatalf("the workspace starts at %s, want origin's feat/v2 %s", head, fresh)
	}
	branches, err := r.service.ListRepositoryBranches(context.Background(), &pb.ListRepositoryBranchesRequest{RepositoryIdentity: added.Repository.GitIdentity})
	if err != nil || branches.StartFrom != "feat/v2" || branches.DefaultBranch != "main" {
		t.Fatalf("remembered: %+v %v", branches, err)
	}

	if out = r.cli(r.owner(), "", "new", "Beta", "--repo", added.Repository.GitIdentity, "--start-from", "feat/typo"); out.code == 0 || !strings.Contains(out.err.Error(), "has no branch feat/typo on origin") {
		t.Fatalf("a branch origin lacks: %+v", out)
	}
}
