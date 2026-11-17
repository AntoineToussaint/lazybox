package telemetry

import (
	"context"
	"path/filepath"
	"testing"
	"time"
)

func TestLatestModelExcludesChildrenAndOrdersReports(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "usage.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	ctx := context.Background()
	sc := Scope{Execution: "exec", Conversation: "conv", Profile: "account", Provider: "claude"}
	for _, r := range []Record{{ID: "old", Scope: sc, Agent: "root", Model: "old", ObservedAt: time.Now().Add(-time.Minute)}, {ID: "new", Scope: sc, Agent: "root", Model: "new", ObservedAt: time.Now()}, {ID: "child", Scope: sc, Agent: "child", Model: "child", ObservedAt: time.Now().Add(time.Minute)}} {
		if err = s.Put(ctx, r); err != nil {
			t.Fatal(err)
		}
	}
	model, err := s.LatestModel(ctx, "exec")
	if err != nil || model != "new" {
		t.Fatal(model, err)
	}
	if model, err = s.LatestModel(ctx, "absent"); err != nil || model != "" {
		t.Fatal(model, err)
	}
}

func TestCommandRunsAreRecordedPerExecution(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage.db")
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	if err = s.PutCommand(ctx, CommandRun{ID: "orphan"}); err == nil {
		t.Fatal("a run without its execution is unattributable")
	}
	sc := Scope{Workspace: "ws", Conversation: "conv", Execution: "exec"}
	for _, c := range []CommandRun{{ID: "a", Scope: sc, Filter: "go-test", ExitCode: 1, OutputBytes: 1 << 20, AgentBytes: 2048}, {ID: "b", Scope: sc, Filter: "passthrough", OutputBytes: 10, AgentBytes: 60}, {ID: "a", Scope: sc, Filter: "go-test", ExitCode: 1, OutputBytes: 1 << 20, AgentBytes: 1024}} {
		if err = s.PutCommand(ctx, c); err != nil {
			t.Fatal(err)
		}
	}
	s.Close()
	if s, err = Open(path); err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	runs, err := s.Commands(ctx, "exec")
	if err != nil || len(runs) != 2 || runs[0].ID != "a" || runs[0].AgentBytes != 1024 || runs[0].Filter != "go-test" || runs[0].ExitCode != 1 || runs[0].Scope != sc || runs[0].ObservedAt.IsZero() || runs[1].ID != "b" {
		t.Fatalf("%v %+v", err, runs)
	}
	if runs, err = s.Commands(ctx, "absent"); err != nil || len(runs) != 0 {
		t.Fatal(runs, err)
	}
}
