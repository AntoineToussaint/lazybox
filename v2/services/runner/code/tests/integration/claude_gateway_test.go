package integration

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
)

// This exercises the real supported SDK and harness against a deterministic
// Messages API gateway. It proves protocol wiring without a paid login; it
// deliberately does NOT claim real Anthropic authentication or model behavior.
func TestClaudeSDKWithDeterministicGateway(t *testing.T) {
	worker := os.Getenv("LAZYBOX_V2_WORKER")
	if worker == "" {
		t.Skip("set LAZYBOX_V2_WORKER after make build-worker to exercise real SDK protocol fixtures")
	}
	if _, err := os.Stat(worker); err != nil {
		t.Fatal(err)
	}
	var mu sync.Mutex
	paths := []string{}
	requestCount := 0
	authOK := true
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		paths = append(paths, r.URL.Path)
		requestCount++
		count := requestCount
		if strings.Contains(r.URL.Path, "/messages") && r.Header.Get("X-Api-Key") != "fixture-api-key" {
			authOK = false
		}
		mu.Unlock()
		if strings.HasSuffix(r.URL.Path, "count_tokens") {
			w.Header().Set("Content-Type", "application/json")
			_, _ = io.WriteString(w, `{"input_tokens":10}`)
			return
		}
		if !strings.Contains(r.URL.Path, "/messages") {
			w.Header().Set("Content-Type", "application/json")
			_, _ = io.WriteString(w, `{}`)
			return
		}
		var request struct {
			Model    string `json:"model"`
			Stream   bool   `json:"stream"`
			Messages []struct {
				Role    string          `json:"role"`
				Content json.RawMessage `json:"content"`
			} `json:"messages"`
		}
		if err := json.NewDecoder(io.LimitReader(r.Body, 4<<20)).Decode(&request); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		if count > 20 {
			http.Error(w, "fixture request limit exceeded", 400)
			return
		}
		scenario := "hello"
		resolved := false
		for _, m := range request.Messages {
			var blocks []map[string]any
			if json.Unmarshal(m.Content, &blocks) != nil {
				var text string
				_ = json.Unmarshal(m.Content, &text)
				blocks = []map[string]any{{"type": "text", "text": text}}
			}
			for _, block := range blocks {
				if m.Role == "user" && block["type"] == "text" {
					text, _ := block["text"].(string)
					if strings.Contains(text, "BENCH_FIXTURE_QUESTION") {
						scenario = "question"
						resolved = false
					}
					if strings.Contains(text, "BENCH_FIXTURE_COMMAND") {
						scenario = "command"
						resolved = false
					}
				}
				if block["type"] == "tool_result" && block["tool_use_id"] == "fixture-"+scenario {
					resolved = true
				}
			}
		}
		// The SDK can repeat the original prompt after a tool result. Inspect
		// correlated results after selecting the scenario, rather than letting
		// that repeated prompt reset a completed tool to pending.
		for _, message := range request.Messages {
			var blocks []map[string]any
			_ = json.Unmarshal(message.Content, &blocks)
			for _, block := range blocks {
				if block["type"] == "tool_result" && block["tool_use_id"] == "fixture-"+scenario {
					resolved = true
				}
			}
		}
		content := map[string]any{"type": "text", "text": "FIXTURE-HELLO"}
		stop := "end_turn"
		if scenario == "question" && !resolved {
			stop = "tool_use"
			content = map[string]any{"type": "tool_use", "id": "fixture-question", "name": "AskUserQuestion", "input": map[string]any{"questions": []any{map[string]any{"question": "Choose a color", "header": "Color", "multiSelect": false, "options": []any{map[string]string{"label": "Blue", "description": "Blue color"}, map[string]string{"label": "Red", "description": "Red color"}}}}}}
		} else if scenario == "question" {
			content["text"] = "Blue"
		}
		if scenario == "command" && !resolved {
			stop = "tool_use"
			content = map[string]any{"type": "tool_use", "id": "fixture-command", "name": "mcp__lazybox__command", "input": map[string]any{"action": "start", "argv": []string{"/bin/sh", "-c", "printf 'CLAUDE-SHARED-OUTPUT\n'"}}}
		} else if scenario == "command" {
			content["text"] = "Command registered; retained output is available to the agent and viewers."
		}
		message := map[string]any{"id": "fixture-message", "type": "message", "role": "assistant", "model": request.Model, "content": []any{content}, "stop_reason": stop, "stop_sequence": nil, "usage": map[string]int{"input_tokens": 10, "output_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
		if !request.Stream {
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(message)
			return
		}

		w.Header().Set("Content-Type", "text/event-stream")
		send := func(kind string, value map[string]any) {
			value["type"] = kind
			b, _ := json.Marshal(value)
			_, _ = fmt.Fprintf(w, "event: %s\ndata: %s\n\n", kind, b)
			if f, ok := w.(http.Flusher); ok {
				f.Flush()
			}
		}
		initial := map[string]any{}
		for k, v := range message {
			initial[k] = v
		}
		initial["content"] = []any{}
		initial["stop_reason"] = nil
		send("message_start", map[string]any{"message": initial})
		start := map[string]any{"type": "text", "text": ""}
		delta := map[string]any{"type": "text_delta", "text": content["text"]}
		if content["type"] == "tool_use" {
			start = map[string]any{"type": "tool_use", "id": content["id"], "name": content["name"], "input": map[string]any{}}
			args, _ := json.Marshal(content["input"])
			delta = map[string]any{"type": "input_json_delta", "partial_json": string(args)}
		}
		send("content_block_start", map[string]any{"index": 0, "content_block": start})
		send("content_block_delta", map[string]any{"index": 0, "delta": delta})
		send("content_block_stop", map[string]any{"index": 0})
		send("message_delta", map[string]any{"delta": map[string]any{"stop_reason": stop, "stop_sequence": nil}, "usage": map[string]int{"output_tokens": 10}})
		send("message_stop", map[string]any{})
	}))
	defer server.Close()
	t.Setenv("BENCH_FIXTURE_KEY", "fixture-api-key")
	root := t.TempDir()
	b, err := runner.New(runner.Config{State: root, Worker: worker, Profiles: []accounts.Profile{{ID: "fixture", Provider: "claude", Home: filepath.Join(root, "profile"), Isolation: "environment", EnvRefs: map[string]string{"ANTHROPIC_API_KEY": "BENCH_FIXTURE_KEY"}}}, Gateways: []runner.Gateway{{Provider: "claude", Gateway: agent.Gateway{ID: "fixture-meter", URL: server.URL + "/anthropic/execution"}}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	e, err := b.Open("fixture", t.TempDir(), "", accounts.Options{Model: "claude-fixture", Permission: "ask"}, "fixture-meter", "", "")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()
	cursor := uint64(0)
	questions, approvals := 0, 0
	for _, prompt := range []string{"Say hello without tools.", "BENCH_FIXTURE_QUESTION", "BENCH_FIXTURE_COMMAND"} {
		if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: prompt}); err != nil {
			t.Fatal(err)
		}
		for {
			r, err := e.Engine.Events(ctx, cursor)
			if err != nil {
				t.Fatal(err)
			}
			cursor = r.Sequence
			var ev agent.Event
			_ = json.Unmarshal(r.Value, &ev)
			if ev.Type == "session_ready" {
				var metadata struct {
					Version string `json:"version"`
				}
				_ = json.Unmarshal(ev.Data, &metadata)
				if metadata.Version != "" {
					t.Logf("SDK harness version: %s", metadata.Version)
				}
			}
			if ev.Type == "failure" {
				t.Fatalf("real SDK fixture: %s %s %s", ev.Status, ev.Text, ev.Data)
			}
			if ev.Type == "question" {
				questions++
				if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Answer, RequestID: ev.RequestID, Answers: map[string][]string{"Choose a color": {"Blue"}}}); err != nil {
					t.Fatal(err)
				}
			}
			if ev.Type == "approval" {
				approvals++
				if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Approve, RequestID: ev.RequestID, Decision: "accept"}); err != nil {
					t.Fatal(err)
				}
			}
			if ev.Type == "turn_ended" {
				if ev.Status != "completed" {
					t.Fatal(ev.Status)
				}
				break
			}
		}
	}

	if questions != 1 || approvals != 1 {
		t.Fatalf("native callbacks: %d questions, %d approvals", questions, approvals)
	}
	output := b.Commands.List(e.ID)
	if len(output) != 1 {
		t.Fatalf("managed command not registered: %+v", output)
	}
	command, err := b.Commands.Get(output[0].ID)
	if err != nil {
		t.Fatal(err)
	}
	status, err := command.Wait(ctx)
	if err != nil || status.ExitCode == nil || *status.ExitCode != 0 {
		t.Fatalf("%+v %v", status, err)
	}
	data, _, err := command.Read(ctx, 0, 65536)
	if err != nil || string(data) != "CLAUDE-SHARED-OUTPUT\n" {
		t.Fatalf("%q %v", data, err)
	}
	mu.Lock()
	defer mu.Unlock()
	if !authOK {
		t.Fatal("gateway lost provider auth")
	}
	found := false
	for _, path := range paths {
		if path == "/anthropic/execution/v1/messages" {
			found = true
		}
	}
	if !found {
		t.Fatalf("gateway path bypassed: %v", paths)
	}
	t.Log("real SDK, deterministic HTTP gateway: native question/reply, native permission callback, managed tool execution, shared output, gateway path and provider credential forwarding verified")
}
