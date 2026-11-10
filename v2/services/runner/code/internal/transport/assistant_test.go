package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"strings"
	"testing"
	"time"
)

func TestAssistantUsesSharedProviderWithoutRepositoryOrCommandAuthority(t *testing.T) {
	s, a := collectionService(t)
	ctx := context.Background()
	response, err := s.OpenAssistant(ctx, &pb.OpenAssistantRequest{ProfileId: "claude-default", Purpose: "planning", Context: "Design the release flow"})
	if err != nil {
		t.Fatal(err)
	}
	native := a.Session.Open
	if native.Purpose != "planning" || native.Tools != nil || native.Permission != "read-only" || !strings.Contains(native.Context, "Design the release flow") || response.Session.CollectionId != "" || response.Session.Purpose != "planning" || response.Session.Capabilities.Files {
		t.Fatalf("bad assistant scope: %+v %+v", native, response.Session)
	}
	execution, err := s.Bench.Get(response.Session.Id)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = s.Bench.Start(execution, "must not start", []string{"/bin/sh", "-c", "exit 0"}); err == nil {
		t.Fatal("assistant can start commands")
	}
	a.Session.Events <- agent.Event{Type: "turn_started"}
	a.Session.Events <- agent.Event{Type: "agent_status", AgentID: "child", Name: "Review", Status: "running"}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	deadline := time.Now().Add(time.Second * 3)
	for {
		snap := wireSession(execution)
		if len(snap.Agents) == 2 && snap.Agents[0].Status == "done" {
			if snap.Status != "running" {
				t.Fatal("child work declared done")
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("events not reduced")
		}
		time.Sleep(time.Millisecond)
	}
	if _, err = s.OpenAssistant(ctx, &pb.OpenAssistantRequest{ProfileId: "claude-default", Purpose: "execute"}); err == nil {
		t.Fatal("unbounded assistant purpose accepted")
	}
	if _, err = s.ContinueSession(ctx, &pb.ContinueSessionRequest{SessionId: execution.ID, Mode: "native", Reason: "try"}); err == nil {
		t.Fatal("assistant downgraded into coding authority")
	}
}
