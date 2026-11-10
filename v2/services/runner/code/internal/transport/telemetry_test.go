package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"testing"
)

func TestTelemetryWirePreservesUnknownCountsAndWorkspaceAttribution(t *testing.T) {
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	s := Service{Bench: b}
	scope := telemetry.Scope{Workspace: "w", Conversation: "c", Execution: "e", Profile: "p", Provider: "claude"}
	zero := int64(0)
	input := int64(123)
	reserve := int64(100)
	if err = b.Telemetry.Put(context.Background(), telemetry.Record{ID: "request", Scope: scope, Source: "gateway", Status: "partial", Tokens: telemetry.Tokens{Input: &input, Output: &zero}}); err != nil {
		t.Fatal(err)
	}
	if err = b.Telemetry.PutContext(context.Background(), telemetry.ContextSnapshot{Scope: scope, Agent: "root", Used: &input, Reserve: &reserve, Categories: []telemetry.Category{{Name: "tools_deferred", Kind: "deferred", Tokens: &reserve}}}); err != nil {
		t.Fatal(err)
	}
	result, err := s.GetTelemetry(context.Background(), &pb.TelemetryRequest{Workspace: "w"})
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Groups) != 1 || result.Groups[0].Nanos != nil || result.Groups[0].Tokens.Output == nil || result.Groups[0].Tokens.CacheRead != nil || result.Contexts[0].Reserve == nil || result.Contexts[0].Categories[0].Included {
		t.Fatalf("%+v", result)
	}
	if _, err = s.GetTelemetry(context.Background(), &pb.TelemetryRequest{}); err == nil {
		t.Fatal("unscoped report accepted")
	}
	conversation, err := s.GetTelemetry(context.Background(), &pb.TelemetryRequest{Conversation: "c"})
	if err != nil || len(conversation.Groups) != 1 {
		t.Fatal(conversation, err)
	}
	if _, err = s.GetTelemetry(context.Background(), &pb.TelemetryRequest{Conversation: "c", Execution: "e"}); err == nil {
		t.Fatal("ambiguous scope accepted")
	}
	info, err := s.Info(context.Background(), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(info.Gateways) != 2 {
		t.Fatal("meter options unavailable")
	}
}
