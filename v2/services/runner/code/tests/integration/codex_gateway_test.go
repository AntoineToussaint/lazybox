package integration

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
)

func TestCodexGatewayUsesHTTPResponsesAndExplicitCredential(t *testing.T) {
	for _, purpose := range []string{"coding", "help", "authentication"} {
		t.Run(purpose, func(t *testing.T) { codexGatewayFixture(t, purpose) })
	}
}
func codexGatewayFixture(t *testing.T, purpose string) {
	if _, err := exec.LookPath("codex"); err != nil {
		t.Skip("installed Codex harness required for gateway protocol fixture")
	}
	t.Setenv("BENCH_GATEWAY_FIXTURE_TOKEN", "fixture-gateway-token")
	type observation struct {
		path, auth, upgrade string
		stream              bool
		tools               json.RawMessage
	}
	seen := make(chan observation, 4)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Stream bool            `json:"stream"`
			Tools  json.RawMessage `json:"tools"`
		}
		_ = json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&body)
		select {
		case seen <- observation{r.URL.Path, r.Header.Get("Authorization"), r.Header.Get("Upgrade"), body.Stream, body.Tools}:
		default:
		}
		// A deliberate provider refusal bounds the fixture to routing/auth. It
		// must not be misreported as a completed real model turn.
		w.Header().Set("Content-Type", "application/json")
		if purpose == "authentication" {
			w.WriteHeader(401)
		} else {
			w.WriteHeader(400)
		}
		_, _ = io.WriteString(w, `{"error":{"message":"deterministic gateway refusal","type":"invalid_request_error","code":"fixture_refusal"}}`)
	}))
	defer server.Close()
	root := t.TempDir()
	if err := os.MkdirAll(filepath.Join(root, "profile"), 0700); err != nil {
		t.Fatal(err)
	}
	b, err := runner.New(runner.Config{State: root, Profiles: []accounts.Profile{{ID: "fixture", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "profile"), EnvRefs: map[string]string{"BENCH_GATEWAY_FIXTURE_TOKEN": "BENCH_GATEWAY_FIXTURE_TOKEN"}}}, Gateways: []runner.Gateway{{Provider: "codex", Gateway: agent.Gateway{ID: "fixture-gateway", URL: server.URL + "/openai/execution", TokenEnv: "BENCH_GATEWAY_FIXTURE_TOKEN"}}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	var e *runner.Execution
	if purpose == "help" {
		e, err = b.OpenAssistant("fixture", "help", "Fixture context", accounts.Options{Model: "codex-fixture"}, "fixture-gateway")
	} else {
		e, err = b.Open("fixture", t.TempDir(), "", accounts.Options{Model: "codex-fixture"}, "fixture-gateway", "", "")
	}
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "gateway-fixture", Kind: agent.Message, Text: "Routing fixture; do not use tools."}); err != nil {
		t.Fatal(err)
	}
	select {
	case request := <-seen:
		if purpose == "help" {
			var tools any
			_ = json.Unmarshal(request.tools, &tools)
			var names []string
			var visit func(any)
			visit = func(value any) {
				switch v := value.(type) {
				case map[string]any:
					for k, x := range v {
						if k == "name" {
							if name, ok := x.(string); ok {
								names = append(names, name)
							}
						}
						visit(x)
					}
				case []any:
					for _, x := range v {
						visit(x)
					}
				}
			}
			visit(tools)
			t.Logf("assistant advertised tools: %v", names)
			for _, name := range names {
				switch name {
				case "exec_command", "write_stdin", "shell", "shell_command", "apply_patch", "lazybox_command", "spawn_agent", "functions.exec", "functions.exec_command", "create_goal", "update_goal":
					t.Errorf("assistant exposes execution tool %s", name)
				}
			}
		}
		if purpose == "authentication" {
			for {
				snapshot := e.Engine.Snapshot()
				if snapshot.Account != nil && snapshot.Account.State == "auth_required" {
					break
				}
				select {
				case <-ctx.Done():
					t.Fatalf("native 401 did not become auth_required: %+v", snapshot)
				case <-time.After(time.Millisecond * 10):
				}
			}
		}
		if request.path != "/openai/execution/responses" || request.auth != "Bearer fixture-gateway-token" || request.upgrade != "" || !request.stream {
			t.Fatalf("incorrect routing: path=%s upgrade=%s stream=%v", request.path, request.upgrade, request.stream)
		}
	case <-ctx.Done():
		t.Fatal("Codex did not call the configured HTTP gateway")
	}
}
