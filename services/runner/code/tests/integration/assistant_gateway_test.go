package integration

import (
	"context"
	"encoding/json"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"
)

// Application assistants advertise no built-in or ambient MCP tools. The help
// assistant advertises exactly Lazybox's settings tools; the others nothing.
func TestClaudeAssistantAdvertisesOnlyItsLazyboxTools(t *testing.T) {
	for purpose, want := range map[string]string{"help": "mcp__lazybox_settings__get_settings,mcp__lazybox_settings__read_doc,mcp__lazybox_settings__search_docs,mcp__lazybox_settings__update_settings", "planning": ""} {
		t.Run(purpose, func(t *testing.T) { claudeAssistantTools(t, purpose, want) })
	}
}

func claudeAssistantTools(t *testing.T, purpose, want string) {
	worker := os.Getenv("LAZYBOX_V2_WORKER")
	if worker == "" {
		t.Skip("build-worker required")
	}
	observed := make(chan string, 16)
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
			Tools []struct {
				Name         string `json:"name"`
				DeferLoading bool   `json:"defer_loading"`
			} `json:"tools"`
		}
		raw, _ := io.ReadAll(io.LimitReader(r.Body, 4<<20))
		_ = json.Unmarshal(raw, &body)
		// Only the conversation turn carries the assistant context; the SDK
		// also sends side requests (such as naming the session) with no tools.
		if strings.Contains(string(raw), "Use only supplied context") {
			names := []string{}
			for _, tool := range body.Tools {
				// The CLI's reserved, never-callable marker for deferred tool loading.
				if tool.Name == "DeferredToolPlaceholder" && tool.DeferLoading {
					continue
				}
				names = append(names, tool.Name)
			}
			sort.Strings(names)
			select {
			case observed <- strings.Join(names, ","):
			default:
			}
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
	e, err := b.OpenAssistant("fixture", purpose, "Use only supplied context", accounts.Options{}, "fixture-gateway")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: purpose, Kind: agent.Message, Text: "What is a Collection?"}); err != nil {
		t.Fatal(err)
	}
	select {
	case names := <-observed:
		if names != want {
			t.Fatalf("%s assistant advertised tools %q, want %q", purpose, names, want)
		}
	case <-ctx.Done():
		t.Fatal("SDK did not reach local fixture")
	}
}

// scriptedToolUse answers a Messages request with one streamed tool call.
func scriptedToolUse(w http.ResponseWriter, id, name, input string) {
	w.Header().Set("Content-Type", "text/event-stream")
	start, _ := json.Marshal(map[string]any{"type": "content_block_start", "index": 0, "content_block": map[string]any{"type": "tool_use", "id": id, "name": name, "input": map[string]any{}}})
	delta, _ := json.Marshal(map[string]any{"type": "content_block_delta", "index": 0, "delta": map[string]any{"type": "input_json_delta", "partial_json": input}})
	for _, event := range []string{
		`{"type":"message_start","message":{"id":"msg_` + id + `","type":"message","role":"assistant","model":"claude-fixture","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":1}}}`,
		string(start), string(delta),
		`{"type":"content_block_stop","index":0}`,
		`{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":5}}`,
		`{"type":"message_stop"}`,
	} {
		var kind struct{ Type string }
		_ = json.Unmarshal([]byte(event), &kind)
		_, _ = io.WriteString(w, "event: "+kind.Type+"\ndata: "+event+"\n\n")
	}
}

// The help assistant reads Lazybox's settings without asking, but its proposed
// change waits for the user — even under a YOLO Runner default — and the Runner
// applies it only once allowed. A scripted local Messages endpoint stands in
// for the model: no inference, fixture credentials only.
func TestClaudeHelpAssistantChangesSettingsOnlyWithApproval(t *testing.T) {
	worker := os.Getenv("LAZYBOX_V2_WORKER")
	if worker == "" {
		t.Skip("build-worker required")
	}
	proposal := `{"revision":2,"reason":"You asked new coding sessions to ask before running tools.","changes":{"permission":"ask"}}`
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	results := make(chan string, 4)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(io.LimitReader(r.Body, 4<<20))
		if !strings.Contains(r.URL.Path, "/messages") || strings.HasSuffix(r.URL.Path, "count_tokens") || !strings.Contains(string(raw), "Use only supplied context") {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(400)
			_, _ = io.WriteString(w, `{"type":"error","error":{"type":"invalid_request_error","message":"side request refused by fixture"}}`)
			return
		}
		var body struct {
			Messages []struct {
				Content json.RawMessage `json:"content"`
			} `json:"messages"`
		}
		_ = json.Unmarshal(raw, &body)
		var last []struct {
			Type    string          `json:"type"`
			Content json.RawMessage `json:"content"`
		}
		if n := len(body.Messages); n > 0 {
			_ = json.Unmarshal(body.Messages[n-1].Content, &last)
		}
		for _, block := range last {
			if block.Type == "tool_result" {
				select {
				case results <- string(block.Content):
				case <-ctx.Done():
					return
				case <-r.Context().Done():
					return
				}
			}
		}
		switch strings.Count(string(raw), `"tool_use_id"`) {
		case 0:
			scriptedToolUse(w, "toolu_read", "mcp__lazybox_settings__get_settings", `{}`)
		case 1:
			scriptedToolUse(w, "toolu_update", "mcp__lazybox_settings__update_settings", proposal)
		default:
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(400)
			_, _ = io.WriteString(w, `{"type":"error","error":{"type":"invalid_request_error","message":"script complete"}}`)
		}
	}))
	defer func() { cancel(); server.Close() }()
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
	d := b.Preferences.Snapshot()
	d.Permission = "yolo"
	if err = b.Preferences.Save(d); err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenAssistant("fixture", "help", "Use only supplied context", accounts.Options{}, "fixture-gateway")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "ask", Kind: agent.Message, Text: "Make new sessions ask before running tools."}); err != nil {
		t.Fatal(err)
	}
	select {
	case read := <-results:
		if !strings.Contains(read, `\"revision\":2`) || !strings.Contains(read, `\"permission\":\"yolo\"`) {
			t.Fatalf("read result: %s", read)
		}
	case <-ctx.Done():
		t.Fatal("settings were not read without approval")
	}
	var request agent.Event
	for request.RequestID == "" {
		for _, pending := range e.Engine.Snapshot().Pending {
			request = pending
		}
		select {
		case <-ctx.Done():
			t.Fatal("settings update did not ask for approval")
		case <-time.After(20 * time.Millisecond):
		}
	}
	if request.Type != "approval" || request.Text != "mcp__lazybox_settings__update_settings" || !strings.Contains(string(request.Data), `"permission":"ask"`) {
		t.Fatalf("approval: %+v %s", request, request.Data)
	}
	if got := b.Preferences.Snapshot().Permission; got != "yolo" {
		t.Fatalf("settings changed before approval: %s", got)
	}
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "allow", Kind: agent.Approve, RequestID: request.RequestID, Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	select {
	case applied := <-results:
		if !strings.Contains(applied, `\"applied\":true`) {
			t.Fatalf("update result: %s", applied)
		}
	case <-ctx.Done():
		t.Fatal("approved update did not complete")
	}
	if got := b.Preferences.Snapshot(); got.Permission != "ask" || got.Revision != 3 {
		t.Fatalf("approved update not applied: %+v", got)
	}
	cancel()
	// A provider may repeat a result after our assertions finish. A full
	// observation queue must not keep an HTTP handler (and Close) alive.
fill:
	for {
		select {
		case results <- "already observed":
		default:
			break fill
		}
	}
	replayCtx, replayCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer replayCancel()
	replay, err := http.NewRequestWithContext(replayCtx, http.MethodPost, server.URL+"/messages", strings.NewReader(`{"system":"Use only supplied context","messages":[{"content":[{"type":"tool_result","tool_use_id":"toolu_read","content":"replayed"}]}]}`))
	if err != nil {
		t.Fatal(err)
	}
	response, err := server.Client().Do(replay)
	if err != nil {
		t.Fatalf("completed fixture blocked a repeated result: %v", err)
	}
	response.Body.Close()

}
