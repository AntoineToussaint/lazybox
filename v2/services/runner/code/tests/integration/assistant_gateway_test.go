package integration

import (
	"context"
	"encoding/json"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestClaudeAssistantAdvertisesNoBuiltInOrMCPTools(t *testing.T) {
	worker := os.Getenv("LAZYBOX_V2_WORKER")
	if worker == "" {
		t.Skip("build-worker required")
	}
	observed := make(chan int, 16)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		if strings.HasSuffix(r.URL.Path, "count_tokens") {
			_, _ = io.WriteString(w, `{"input_tokens":10}`)
			return
		}
		if !strings.Contains(r.URL.Path, "/messages") {
			_, _ = io.WriteString(w, `{}`)
			return
		}
		var body struct {
			Tools []json.RawMessage `json:"tools"`
		}
		_ = json.NewDecoder(io.LimitReader(r.Body, 4<<20)).Decode(&body)
		select {
		case observed <- len(body.Tools):
		default:
		}
		w.WriteHeader(400)
		_, _ = io.WriteString(w, `{"type":"error","error":{"type":"invalid_request_error","message":"local assistant routing fixture"}}`)
	}))
	defer server.Close()
	t.Setenv("ASSISTANT_FIXTURE_KEY", "fixture-only-key")
	root := t.TempDir()
	home := filepath.Join(root, "claude")
	if err := os.Mkdir(home, 0700); err != nil {
		t.Fatal(err)
	}
	b, err := runner.New(runner.Config{State: root, Worker: worker, Profiles: []accounts.Profile{{ID: "fixture", Provider: "claude", Isolation: "environment", Home: home, EnvRefs: map[string]string{"ANTHROPIC_API_KEY": "ASSISTANT_FIXTURE_KEY"}}}, Gateways: []runner.Gateway{{Provider: "claude", Gateway: agent.Gateway{ID: "fixture-gateway", URL: server.URL}}}})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	e, err := b.OpenAssistant("fixture", "help", "Use only supplied context", accounts.Options{}, "fixture-gateway")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "help", Kind: agent.Message, Text: "What is a Collection?"}); err != nil {
		t.Fatal(err)
	}
	select {
	case count := <-observed:
		if count != 0 {
			t.Fatalf("assistant advertised %d tools", count)
		}
	case <-ctx.Done():
		t.Fatal("SDK did not reach local fixture")
	}
}
