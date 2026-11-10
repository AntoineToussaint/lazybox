package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"testing"
)

func TestRunnerSettingsRPCUsesRevisionsAndExactPriceOverrides(t *testing.T) {
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	s := Service{Bench: b}
	ctx := context.Background()
	initial, err := s.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if initial.Revision != 1 || initial.SuggestedName == "" || initial.Context == "" || initial.Permission != "" {
		t.Fatal(initial)
	}
	initial.Permission = "yolo"
	initial.Context = "Common"
	initial.CodexContext = "Codex"
	initial.Naming = "generated"
	updated, err := s.SaveRunnerSettings(ctx, initial)
	if err != nil {
		t.Fatal(err)
	}
	if updated.Revision != 2 || updated.Context != "Common" || updated.CodexContext != "Codex" {
		t.Fatal(updated)
	}
	if _, err = s.SaveRunnerSettings(ctx, initial); status.Code(err) != codes.Aborted {
		t.Fatal("lost-update protection", err)
	}
	updated, err = s.SetModelPrice(ctx, &pb.SetModelPriceRequest{Revision: updated.Revision, Price: &pb.ModelPrice{Provider: "claude", Model: "claude-fable-5-1", Input: 1234, Output: 5678, CacheRead: 100}})
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, p := range updated.Prices {
		if p.Model == "claude-fable-5-1" {
			found = p.Custom && p.Input == 1234 && p.Output == 5678 && p.CacheRead == 100 && p.Version != ""
		}
	}
	if !found {
		t.Fatal("exact override absent", updated)
	}
	restored, err := s.SetModelPrice(ctx, &pb.SetModelPriceRequest{Revision: updated.Revision, Price: &pb.ModelPrice{Provider: "claude", Model: "claude-fable-5-1"}, Remove: true})
	if err != nil {
		t.Fatal(err)
	}
	for _, p := range restored.Prices {
		if p.Model == "claude-fable-5-1" && (p.Custom || p.Input != 10000) {
			t.Fatal("base rate not restored", p)
		}
	}
	if _, err = s.SetModelPrice(ctx, &pb.SetModelPriceRequest{Revision: restored.Revision}); err == nil {
		t.Fatal("missing rate accepted")
	}
}
