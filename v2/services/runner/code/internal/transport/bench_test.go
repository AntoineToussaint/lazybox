package transport

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

func TestCheckpointSwitchKeepsWorkIdentityAndSiblingCredentials(t *testing.T) {
	t.Setenv("TEST_PROFILE_A", "credential-a")
	t.Setenv("TEST_PROFILE_B", "credential-b")
	root := t.TempDir()
	profiles := []accounts.Profile{{ID: "a", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "a"), EnvRefs: map[string]string{"OPENAI_API_KEY": "TEST_PROFILE_A"}}, {ID: "b", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "b"), EnvRefs: map[string]string{"OPENAI_API_KEY": "TEST_PROFILE_B"}}}
	b, err := runner.New(runner.Config{State: root, Profiles: profiles})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	a := &agenttest.Adapter{}
	b.Adapters["codex"] = a
	first, err := b.Open("a", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	sibling, err := b.Open("a", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	originalEnv := append([]string{}, sibling.Env...)
	service := &Service{Bench: b}
	if _, err = service.ContinueSession(context.Background(), &pb.ContinueSessionRequest{SessionId: first.ID, ProfileId: "b", Mode: "native", Reason: "capacity exhausted"}); err == nil {
		t.Fatal("unverified cross-account native resume advertised")
	}
	next, err := service.ContinueSession(context.Background(), &pb.ContinueSessionRequest{SessionId: first.ID, ProfileId: "b", Mode: "checkpoint", Reason: "simulated capacity exhaustion (not a real account observation)", Checkpoint: &pb.Checkpoint{Objective: "continue work", Criteria: []string{"complete result"}, SourceRevisions: []string{"repo@pinned-revision"}, WorkingTreeState: "inspected, no changes", OmittedContext: []string{"provider-private reasoning"}}})
	if err != nil {
		t.Fatal(err)
	}
	if next.Session.WorkId != first.WorkID || next.Session.ProfileId != "b" || next.Session.PreviousExecutionId != first.ID || !first.Engine.Snapshot().Closed {
		t.Fatalf("invalid continuation %+v", next.Session)
	}
	predecessor, err := service.GetSession(context.Background(), &pb.SessionRequest{SessionId: first.ID})
	if err != nil || predecessor.Session.SuccessorExecutionId != next.Session.Id {
		t.Fatal(predecessor, err)
	}
	if sibling.Engine.Snapshot().Closed {
		t.Fatal("switch closed sibling")
	}
	if len(originalEnv) != len(sibling.Env) {
		t.Fatal("sibling env changed")
	}
	for i := range originalEnv {
		if originalEnv[i] != sibling.Env[i] {
			t.Fatal("sibling credential context mutated")
		}
	}
	if _, err = service.ContinueSession(context.Background(), &pb.ContinueSessionRequest{SessionId: first.ID, ProfileId: "b", Mode: "checkpoint", Reason: "duplicate"}); err == nil {
		t.Fatal("started a second successor from a closed writer")
	}
}

func TestNativeResumeAndCheckpointValidation(t *testing.T) {
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	a := &agenttest.Adapter{}
	b.Adapters["codex"] = a
	e, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	s := &Service{Bench: b}
	if _, err = s.Apply(context.Background(), &pb.ApplyRequest{SessionId: e.ID, OperationId: "untyped"}); err == nil {
		t.Fatal("untyped operation accepted")
	}
	if _, err = s.ContinueSession(context.Background(), &pb.ContinueSessionRequest{SessionId: e.ID, Mode: "checkpoint", Reason: "handoff"}); err == nil {
		t.Fatal("no checkpoint accepted")
	}
	r, err := s.ContinueSession(context.Background(), &pb.ContinueSessionRequest{SessionId: e.ID, Mode: "native", Reason: "adapter restart"})
	if err != nil {
		t.Fatal(err)
	}
	if a.Session.Open.Resume != "native-reference" || r.Session.WorkId != e.WorkID {
		t.Fatal("provider resume identity lost")
	}
}

func TestReadOnlyExecutionCannotBypassManagedCommandOwner(t *testing.T) {
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{Permission: "read-only"}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.Start(e, "not allowed", []string{"/bin/sh", "-c", "echo unsafe"}); err == nil {
		t.Fatal("read-only execution started a managed process")
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "empty", Kind: agent.Message}); err == nil {
		t.Fatal("empty message accepted")
	}
}

func TestDefaultSessionKeepsNativePermissionAndCurrentProfile(t *testing.T) {
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	a := &agenttest.Adapter{}
	b.Adapters["claude"] = a
	s := &Service{Bench: b}
	r, err := s.OpenSession(context.Background(), &pb.OpenSessionRequest{ProfileId: "claude-default", Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	if a.Session.Open.Permission != "" || a.Session.Open.Profile != "claude-default" || a.Session.Open.Gateway != nil || r.Session.ProfileId != "claude-default" {
		t.Fatal("default session overrides configured native permissions, account, or route")
	}
}
