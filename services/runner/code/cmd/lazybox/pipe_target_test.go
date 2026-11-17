package main

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// From a person's own terminal, `lazybox run` and `lazybox log` find the
// Runner the power verbs find, and the workspace by name or by the current
// directory; inside Lazybox they keep their own scoped endpoint.
func TestRunAndLogFindTheRunnerAndWorkspaceFromAPersonsTerminal(t *testing.T) {
	r := newTestRunner(t)
	r.register("lazybox")
	home := func(env map[string]string) func(string) string {
		return func(k string) string {
			if k == "HOME" {
				return r.home
			}
			return env[k]
		}
	}
	if got := ownerEndpoint(home(nil), "", ""); got != r.endpoint {
		t.Fatalf("own terminal: %q, want the registered Runner's %q", got, r.endpoint)
	}
	if got := ownerEndpoint(home(map[string]string{"LAZYBOX_V2_COLLECTION_ID": "c"}), "", ""); got != "" {
		t.Fatalf("a workspace terminal borrowed the owner's endpoint: %q", got)
	}
	if got := ownerEndpoint(home(nil), "/explicit.json", ""); got != "/explicit.json" {
		t.Fatal(got)
	}

	client := genconnect.NewRunnerServiceClient(newHTTPClient(testOwnerToken), r.url)
	ctx := context.Background()
	id := r.collection.Id
	for given, want := range map[string]string{id: id, "Alpha feature": id, "alpha FEATURE": id, "unknown": "unknown"} {
		if got, err := resolveWorkspace(ctx, client, given, ""); err != nil || got != want {
			t.Errorf("%q → %q %v, want %q", given, got, err, want)
		}
	}
	worktree := r.collection.Members[0].Path
	if got, _ := resolveWorkspace(ctx, client, "", filepath.Join(worktree, "src", "deep")); got != id {
		t.Fatalf("inside the worktree: %q, want %q", got, id)
	}
	if got, _ := resolveWorkspace(ctx, client, "", t.TempDir()); got != "" {
		t.Fatalf("outside every workspace: %q", got)
	}
}
