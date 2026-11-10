package transport

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

func TestEmptyWorkspacesKeepSelectedAccountsFilesAndNativePermissions(t *testing.T) {
	root := t.TempDir()
	profiles := []accounts.Profile{}
	t.Setenv("CLAUDE_CONFIG_DIR", t.TempDir())
	for _, id := range []string{"personal", "work"} {
		p, err := accounts.PrepareLogin(root, id, "claude", id)
		if err != nil {
			t.Fatal(err)
		}
		profiles = append(profiles, p)
	}
	bench, err := runner.New(runner.Config{State: root, Profiles: profiles})
	if err != nil {
		t.Fatal(err)
	}
	defer bench.Close()
	adapter := &agenttest.Adapter{}
	bench.Adapters["claude"] = adapter
	service := &Service{Bench: bench}
	paths := map[string]bool{}
	for _, profile := range profiles {
		response, err := service.OpenSession(context.Background(), &pb.OpenSessionRequest{ProfileId: profile.ID, EmptyWorkspace: true})
		if err != nil {
			t.Fatal(err)
		}
		session := response.Session
		expected := filepath.Join(root, "workspaces", session.Id)
		if session.ProfileId != profile.ID || session.WorkingDirectory != expected || session.CollectionId != "" || paths[expected] {
			t.Fatal("wrong account or shared scratch workspace")
		}
		paths[expected] = true
		entries, err := os.ReadDir(expected)
		if err != nil || len(entries) != 0 {
			t.Fatal("workspace was not empty", err)
		}
		if adapter.Session.Open.Permission != "" || adapter.Session.Open.Gateway != nil || adapter.Session.Open.Purpose != "coding" {
			t.Fatal("native execution options changed")
		}
		if err = os.WriteFile(filepath.Join(expected, "keep.txt"), []byte(profile.ID), 0600); err != nil {
			t.Fatal(err)
		}
		if _, err = service.CloseSession(context.Background(), &pb.SessionRequest{SessionId: session.Id}); err != nil {
			t.Fatal(err)
		}
		if _, err = os.Stat(filepath.Join(expected, "keep.txt")); err != nil {
			t.Fatal("session close deleted workspace files")
		}
	}
	for _, request := range []*pb.OpenSessionRequest{{ProfileId: "personal", EmptyWorkspace: true, Cwd: root}, {ProfileId: "personal", EmptyWorkspace: true, CollectionId: "anything"}, {ProfileId: "personal", EmptyWorkspace: true, ResumeReference: "native"}, {ProfileId: "missing", EmptyWorkspace: true}} {
		if _, err = service.OpenSession(context.Background(), request); err == nil {
			t.Fatal("invalid empty-workspace request accepted")
		}
	}
	entries, _ := os.ReadDir(filepath.Join(root, "workspaces"))
	if len(entries) != 2 {
		t.Fatal("invalid requests allocated directories")
	}
}
