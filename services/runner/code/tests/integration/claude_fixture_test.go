package integration

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"testing"
)

// fixtureTurn is what a deterministic Messages endpoint sees of one request:
// every text block that is not the assistant's, and every tool result so far.
type fixtureTurn struct {
	Texts   []string
	Results map[string]bool
}

func (t fixtureTurn) said(text string) bool {
	for _, s := range t.Texts {
		if strings.Contains(s, text) {
			return true
		}
	}
	return false
}

// fixtureReply is the assistant's one content block: text, or a tool call.
type fixtureReply struct {
	Text      string
	ToolID    string
	ToolName  string
	ToolInput map[string]any
}

// claudeMessagesFixture serves the Messages API for the real Claude Code CLI
// the SDK worker starts: each request is answered by respond with one block,
// streamed as the API streams it. It proves the CLI's own behaviour against
// Lazybox without a login and without inference; it claims nothing about a
// real model. The worker must be built (LAZYBOX_V2_WORKER).
func claudeMessagesFixture(t *testing.T, respond func(fixtureTurn) fixtureReply) (string, string) {
	t.Helper()
	worker := os.Getenv("LAZYBOX_V2_WORKER")
	if worker == "" {
		t.Skip("set LAZYBOX_V2_WORKER after make build-worker to exercise real SDK protocol fixtures")
	}
	if _, err := os.Stat(worker); err != nil {
		t.Fatal(err)
	}
	var mu sync.Mutex
	count := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
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
		mu.Lock()
		count++
		over := count > 40
		mu.Unlock()
		if over {
			http.Error(w, "fixture request limit exceeded", 400)
			return
		}
		seen := fixtureTurn{Results: map[string]bool{}}
		for _, m := range request.Messages {
			var blocks []map[string]any
			if json.Unmarshal(m.Content, &blocks) != nil {
				var text string
				_ = json.Unmarshal(m.Content, &text)
				blocks = []map[string]any{{"type": "text", "text": text}}
			}
			for _, block := range blocks {
				// The CLI surfaces a message steered into the turn as a
				// system block beside the tool result; replies are not input.
				if m.Role == "assistant" {
					continue
				}
				switch block["type"] {
				case "text":
					text, _ := block["text"].(string)
					seen.Texts = append(seen.Texts, text)
				case "tool_result":
					id, _ := block["tool_use_id"].(string)
					seen.Results[id] = true
					// The CLI folds text it adds beside a result (a message
					// steered into the turn, reminders) into the result.
					seen.Texts = append(seen.Texts, resultTexts(block["content"])...)
				}
			}
		}
		reply := respond(seen)
		content := map[string]any{"type": "text", "text": reply.Text}
		stop := "end_turn"
		if reply.ToolID != "" {
			stop = "tool_use"
			content = map[string]any{"type": "tool_use", "id": reply.ToolID, "name": reply.ToolName, "input": reply.ToolInput}
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
		delta := map[string]any{"type": "text_delta", "text": reply.Text}
		if reply.ToolID != "" {
			start = map[string]any{"type": "tool_use", "id": reply.ToolID, "name": reply.ToolName, "input": map[string]any{}}
			args, _ := json.Marshal(reply.ToolInput)
			delta = map[string]any{"type": "input_json_delta", "partial_json": string(args)}
		}
		send("content_block_start", map[string]any{"index": 0, "content_block": start})
		send("content_block_delta", map[string]any{"index": 0, "delta": delta})
		send("content_block_stop", map[string]any{"index": 0})
		send("message_delta", map[string]any{"delta": map[string]any{"stop_reason": stop, "stop_sequence": nil}, "usage": map[string]int{"output_tokens": 10}})
		send("message_stop", map[string]any{})
	}))
	t.Cleanup(server.Close)
	return worker, server.URL
}

// resultTexts are the text parts of a tool result's content.
func resultTexts(content any) []string {
	switch c := content.(type) {
	case string:
		return []string{c}
	case []any:
		var out []string
		for _, part := range c {
			if p, ok := part.(map[string]any); ok && p["type"] == "text" {
				if text, ok := p["text"].(string); ok {
					out = append(out, text)
				}
			}
		}
		return out
	}
	return nil
}
