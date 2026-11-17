package transport

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func writeProcessFile(t *testing.T, worktree, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Join(worktree, ".lazybox"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(worktree, ".lazybox", "process.yaml"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

// The effective process of a workspace layers the presets, the user's file
// in the Runner's state and the repository's committed file; the user's
// ceiling binds the repository, and the problem is reported at its line.
func TestOrchestrationProcessLayersAWorkspace(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()

	alone, err := s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if alone.Preset != "solo-pair" || !alone.Valid || len(alone.Layers) != 1 || alone.UserFile != filepath.Join(s.Bench.Config.State, "process.yaml") || !strings.HasPrefix(alone.Yaml, "# yaml-language-server: $schema=") {
		t.Fatalf("presets alone: %+v", alone)
	}
	if many, err := s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{Tasks: 3}); err != nil || many.Preset != "reviewed-team" {
		t.Fatalf("several tasks: %v %v", many.GetPreset(), err)
	}

	if err = os.WriteFile(s.Bench.Config.ProcessFile, []byte("version: 1\nceiling: {autonomy: 3}\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	c := createTestCollection(t, s)
	writeProcessFile(t, c.Members[0].Path, "version: 1\nextends: reviewed-team\n")
	got, err := s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{CollectionId: c.Id})
	if err != nil {
		t.Fatal(err)
	}
	if got.Valid || got.Preset != "reviewed-team" || len(got.Layers) != 3 || len(got.Candidates) != 1 || got.Notice != "" {
		t.Fatalf("layered: %+v", got)
	}
	var ceiling struct{ Autonomy int }
	if err = json.Unmarshal([]byte(got.CeilingJson), &ceiling); err != nil || ceiling.Autonomy != 3 {
		t.Fatalf("ceiling %s: %v", got.CeilingJson, err)
	}
	found := false
	for _, i := range got.Issues {
		if i.Path == "autonomy" && i.Severity == "error" && strings.Contains(i.Message, "above your ceiling (3)") {
			found = true
		}
	}
	if !found {
		t.Fatalf("ceiling not enforced: %+v", got.Issues)
	}
	var effective map[string]any
	if err = json.Unmarshal([]byte(got.Json), &effective); err != nil || effective["name"] != "reviewed-team" {
		t.Fatalf("json: %v %v", effective["name"], err)
	}

	// With two repositories carrying a file, none applies until one is chosen.
	writeProcessFile(t, c.Members[1].Path, "version: 1\nautonomy: 3\n")
	got, err = s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{CollectionId: c.Id})
	if err != nil || len(got.Candidates) != 2 || !strings.Contains(got.Notice, "api, api-next") || len(got.Layers) != 2 || !got.Valid {
		t.Fatalf("two candidates: %+v %v", got, err)
	}
	got, err = s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{CollectionId: c.Id, Repository: "api-next"})
	if err != nil || len(got.Layers) != 3 || got.Layers[2].Source != "api-next/.lazybox/process.yaml" || !got.Valid || got.Notice != "" {
		t.Fatalf("chosen: %+v %v", got, err)
	}
	// Both members check out repository id "api"; the member name wins.
	if got, err = s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{CollectionId: c.Id, Repository: "api"}); err != nil || got.Layers[2].Source != "api/.lazybox/process.yaml" {
		t.Fatalf("a member name must select that member: %+v %v", got.GetLayers(), err)
	}
	// A repository id two members check out names neither.
	twins, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: "twins", Spec: &pb.CollectionSpec{Name: "Twins", Members: []*pb.WorktreeSpec{{Name: "left", RepositoryId: "api"}, {Name: "right", RepositoryId: "api"}}}})
	if err != nil {
		t.Fatal(err)
	}
	for _, m := range twins.Collection.Members {
		writeProcessFile(t, m.Path, "version: 1\n")
	}
	if _, err = s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{CollectionId: twins.Collection.Id, Repository: "api"}); status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "name one of them") {
		t.Fatalf("an id two members check out must be refused as ambiguous: %v", err)
	}
	if _, err = s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{CollectionId: c.Id, Repository: "web"}); status.Code(err) != codes.NotFound {
		t.Fatalf("unknown repository: %v", err)
	}
	if _, err = s.GetOrchestrationProcess(ctx, &pb.OrchestrationProcessRequest{CollectionId: "nope"}); status.Code(err) != codes.NotFound {
		t.Fatalf("unknown workspace: %v", err)
	}
}

// A repository's process file is untrusted: a link out of the worktree is
// refused rather than followed.
func TestOrchestrationProcessRefusesALinkOutOfTheWorktree(t *testing.T) {
	s, _ := collectionService(t)
	c := createTestCollection(t, s)
	secret := filepath.Join(t.TempDir(), "secret")
	if err := os.WriteFile(secret, []byte("token: hunter2\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(c.Members[0].Path, ".lazybox"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(secret, filepath.Join(c.Members[0].Path, ".lazybox", "process.yaml")); err != nil {
		t.Fatal(err)
	}
	_, err := s.GetOrchestrationProcess(context.Background(), &pb.OrchestrationProcessRequest{CollectionId: c.Id})
	if err == nil || strings.Contains(err.Error(), "hunter2") {
		t.Fatalf("followed a link out of the worktree: %v", err)
	}
}

func TestExplainOrchestrationProcess(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	got, err := s.ExplainOrchestrationProcess(ctx, &pb.ExplainOrchestrationProcessRequest{Process: &pb.OrchestrationProcessRequest{Tasks: 2}, EventType: "task.reported"})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Reactions) != 2 || got.Reactions[0].Summary != "start verifier" || got.Reactions[0].Role != "verifier" || !strings.HasPrefix(got.Text, "On task.reported, in order:") || !got.Valid {
		t.Fatalf("explanation: %+v", got)
	}
	if _, err = s.ExplainOrchestrationProcess(ctx, &pb.ExplainOrchestrationProcessRequest{EventType: "task.exploded"}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("unknown event: %v", err)
	}
}

// Agents and terminals may not read the process: it names what agents may
// do. Both RPCs are the owner's alone.
func TestOrchestrationProcessRPCsAreTheOwnersAlone(t *testing.T) {
	f := newTerminalFixture(t)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err := f.client.GetOrchestrationProcess(bearer(ctx, ownerToken), &pb.OrchestrationProcessRequest{CollectionId: f.mine.Id}); err != nil {
		t.Fatal(err)
	}
	for name, token := range map[string]string{"agent": f.agentToken, "terminal": f.terminalToken} {
		scoped := bearer(ctx, token)
		if _, err := f.client.GetOrchestrationProcess(scoped, &pb.OrchestrationProcessRequest{CollectionId: f.mine.Id}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s credential read the process: %v", name, err)
		}
		if _, err := f.client.ExplainOrchestrationProcess(scoped, &pb.ExplainOrchestrationProcessRequest{EventType: "task.reported"}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s credential explained the process: %v", name, err)
		}
	}
}
