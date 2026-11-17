package transport

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// stoppingFixture opens fake native sessions that can stop one sub-agent.
type stoppingFixture struct{ opened chan *agenttest.Session }

func (a stoppingFixture) Open(_ context.Context, o agent.Open) (agent.Session, error) {
	s := &agenttest.Session{Open: o, Ref: o.Profile + "-native", Events: make(chan agent.Event, 16), Applied: make(chan agent.Operation, 16), StopAgent: true}
	a.opened <- s
	return s, nil
}

// A client stops one sub-agent by the id the session lists; the provider gets
// that sub-agent's native id, and the capability is on the wire.
func TestStopAgentStopsTheListedSubAgentByItsNativeID(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	if err := accounts.StoreProfile(b.Config.State, accounts.Profile{ID: "a", Provider: "codex", Isolation: "environment", Home: filepath.Join(b.Config.State, "accounts", "a", "codex")}); err != nil {
		t.Fatal(err)
	}
	if err := b.ReloadProfiles(); err != nil {
		t.Fatal(err)
	}
	adapter := stoppingFixture{opened: make(chan *agenttest.Session, 2)}
	b.Adapters["codex"] = adapter
	ctx := context.Background()
	opened, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir(), Options: &pb.RunnerOptions{NoRotation: true}})
	if err != nil {
		t.Fatal(err)
	}
	if !opened.Session.GetCapabilities().GetStopAgent() {
		t.Fatal("stop_agent capability not on the wire")
	}
	native := <-adapter.opened
	native.Events <- agent.Event{Type: "agent_status", AgentID: "codex:child", ParentAgentID: "root", NativeID: "child-thread", Status: "running", Name: "Audit"}
	var child string
	until(t, "sub-agent never listed", func() bool {
		got, err := service.GetSession(ctx, &pb.SessionRequest{SessionId: opened.Session.Id})
		if err != nil {
			t.Fatal(err)
		}
		for _, a := range got.Session.Agents {
			if a.Id != "root" && a.NativeId == "child-thread" {
				child = a.Id
			}
		}
		return child != ""
	})
	stop := &pb.ApplyRequest{SessionId: opened.Session.Id, OperationId: "stop-1", Operation: &pb.ApplyRequest_StopAgent{StopAgent: &pb.StopAgent{AgentId: child}}}
	if _, err = service.Apply(ctx, stop); err != nil {
		t.Fatal(err)
	}
	op := <-native.Applied
	if op.Kind != agent.StopAgent || op.AgentID != "codex:child" || op.NativeID != "child-thread" {
		t.Fatalf("stop reached the provider as %+v", op)
	}
	missing := &pb.ApplyRequest{SessionId: opened.Session.Id, OperationId: "stop-2", Operation: &pb.ApplyRequest_StopAgent{StopAgent: &pb.StopAgent{AgentId: "root"}}}
	if _, err = service.Apply(ctx, missing); err == nil {
		t.Fatal("the main agent was stopped as a sub-agent")
	}
}
