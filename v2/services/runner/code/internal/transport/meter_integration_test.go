package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"io"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
)

func TestMeteredWorkspaceSessionsAndAssistantRetainAccountingOnRestart(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"id":"fixture-message","model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}`)
	}))
	defer up.Close()
	root := t.TempDir()
	cfg := runner.Config{State: root, WorktreeRoot: filepath.Join(root, "workspaces"), Profiles: []accounts.Profile{{ID: "one", Provider: "claude", Isolation: "inherited"}, {ID: "two", Provider: "claude", Isolation: "inherited"}}, MeterUpstreams: telemetry.Upstreams{Claude: up.URL}}
	b, err := runner.New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	c, err := b.Collections.Create(context.Background(), "test", collections.Spec{Name: "Workspace meter"})
	if err != nil {
		t.Fatal(err)
	}
	native := &agenttest.Adapter{}
	b.Adapters["claude"] = native
	s := Service{Bench: b}
	first, err := s.OpenSession(context.Background(), &pb.OpenSessionRequest{ProfileId: "one", CollectionId: c.ID, Options: &pb.RunnerOptions{GatewayId: "metered"}})
	if err != nil {
		t.Fatal(err)
	}
	firstURL := native.Session.Open.Gateway.URL
	second, err := s.OpenAssistant(context.Background(), &pb.OpenAssistantRequest{ProfileId: "two", CollectionId: c.ID, Purpose: "planning", Options: &pb.RunnerOptions{GatewayId: "metered"}})
	if err != nil {
		t.Fatal(err)
	}
	if second.Session.CollectionId != c.ID || native.Session.Open.Permission != "read-only" || !strings.Contains(native.Session.Open.Context, "do not execute commands") {
		t.Fatal("assistant attribution or restrictions lost")
	}
	for _, base := range []string{firstURL, native.Session.Open.Gateway.URL} {
		r, err := http.Post(base+"/v1/messages", "application/json", strings.NewReader("{}"))
		if err != nil {
			t.Fatal(err)
		}
		io.Copy(io.Discard, r.Body)
		r.Body.Close()
		if r.StatusCode != 200 {
			t.Fatal(r.StatusCode)
		}
	}
	report, err := s.GetTelemetry(context.Background(), &pb.TelemetryRequest{Workspace: c.ID})
	if err != nil {
		t.Fatal(err)
	}
	if len(report.Groups) != 2 || report.Groups[0].Scope.Conversation == report.Groups[1].Scope.Conversation {
		t.Fatalf("sessions merged: %+v", report)
	}
	if first.Session.GatewayId != "metered" {
		t.Fatal("selection not retained")
	}
	b.Close()
	reopened, err := runner.New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	report, err = (&Service{Bench: reopened}).GetTelemetry(context.Background(), &pb.TelemetryRequest{Workspace: c.ID})
	if err != nil || len(report.Groups) != 2 {
		t.Fatal("usage not durable", err)
	}
}
