package transport

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"connectrpc.com/vanguard/vanguardgrpc"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
)

// longJournal writes, as the Runner journals it, a long agent conversation
// shaped like scripts/fixtures/long-conversation.mjs: per turn, the person's
// message, three tool calls whose outputs are 8, 8 and 32 KB, and an answer
// streamed as 12-character deltas. It stops at `events` records.
func longJournal(t testing.TB, path string, events int) (turnStarts []uint64) {
	t.Helper()
	f, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	w := bufio.NewWriter(f)
	seq := uint64(0)
	at := time.Date(2026, 11, 1, 9, 0, 0, 0, time.UTC)
	add := func(value map[string]any) {
		seq++
		at = at.Add(50 * time.Millisecond)
		raw, _ := json.Marshal(value)
		line, _ := json.Marshal(map[string]any{"sequence": seq, "time": at, "value": json.RawMessage(raw)})
		w.Write(line)
		w.WriteByte('\n')
	}
	add(map[string]any{"type": "session_ready", "data": map[string]any{"model": "claude-fixture-model"}})
	for turn := 0; int(seq) < events; turn++ {
		id := fmt.Sprintf("turn-%d", turn)
		turnStarts = append(turnStarts, seq+1)
		add(map[string]any{"type": "operation_accepted", "kind": "message", "operationId": "op-" + id, "text": fmt.Sprintf("Question %d: please continue with the next step of the refactor and explain it.", turn), "turnId": id})
		add(map[string]any{"type": "turn_started", "turnId": id})
		add(map[string]any{"type": "activity", "status": "thinking", "turnId": id})
		for c := range 3 {
			call := fmt.Sprintf("tool-%d-%d", turn, c)
			size := 8000
			if c == 2 {
				size = 32000
			}
			line := fmt.Sprintf("packages/surfaces/src/transcript-model.ts:%d: export function appendTranscript(state, record) { /* %d */ }\n", c, turn)
			add(map[string]any{"type": "activity", "status": "tool", "name": "Bash", "turnId": id})
			add(map[string]any{"type": "tool_activity", "turnId": id, "data": map[string]any{"type": "tool_call", "id": call, "name": "Bash", "status": "running", "input": map[string]any{"command": "rg -n appendTranscript packages/", "description": "Search the code base"}}})
			add(map[string]any{"type": "tool_activity", "turnId": id, "data": map[string]any{"type": "tool_call", "id": call, "status": "completed", "output": strings.Repeat(line, size/len(line)+1)}})
		}
		add(map[string]any{"type": "activity", "status": "responding", "turnId": id})
		answer := fmt.Sprintf("## Step %d: what changed\n\nI looked at the transcript reader and the journal. The replay path folds each event, then publishes, with enough prose to wrap across several lines in the chat pane.\n\n- The reader keeps a cursor per view.\n- Each delta extends the last message.\n- Tool output stays in the activity panel.\n\n```go\nfunc (s *Service) Watch(ctx context.Context, id string) error {\n\tfor {\n\t\trecord, err := s.journal.Next(ctx, s.cursor)\n\t\tif err != nil {\n\t\t\treturn fmt.Errorf(\"watch %%s: %%w\", id, err)\n\t\t}\n\t\ts.cursor = record.Sequence\n\t}\n}\n```\n\nThat completes turn %d. ANSWER_END_%d", turn, turn, turn)
		for i := 0; i < len(answer); i += 12 {
			add(map[string]any{"type": "message_delta", "text": answer[i:min(i+12, len(answer))], "turnId": id})
		}
		add(map[string]any{"type": "usage", "turnId": id, "data": map[string]any{"model": "claude-fixture-model", "input_tokens": 1200 + turn, "output_tokens": 400}})
		add(map[string]any{"type": "turn_ended", "status": "completed", "turnId": id})
	}
	if err = w.Flush(); err != nil {
		t.Fatal(err)
	}
	return turnStarts
}

// connectHost serves the Runner's API the way the app reaches it: Connect
// over HTTP, transcoded to the gRPC service.
func connectHost(t testing.TB, b *runner.Bench) *httptest.Server {
	t.Helper()
	server := grpc.NewServer()
	pb.RegisterRunnerServiceServer(server, &Service{Bench: b})
	transcoder, err := vanguardgrpc.NewTranscoder(server)
	if err != nil {
		t.Fatal(err)
	}
	host := httptest.NewServer(transcoder)
	t.Cleanup(host.Close)
	return host
}

// connectJSON calls a Connect method with a JSON body the way the browser
// client does, and returns the response body and when its first byte came.
func connectJSON(t testing.TB, host, method string, body any, stream bool) (data []byte, first time.Duration, total time.Duration) {
	t.Helper()
	payload, _ := json.Marshal(body)
	contentType := "application/json"
	if stream {
		envelope := make([]byte, 5)
		binary.BigEndian.PutUint32(envelope[1:], uint32(len(payload)))
		payload = append(envelope, payload...)
		contentType = "application/connect+json"
	}
	started := time.Now()
	req, _ := http.NewRequestWithContext(context.Background(), http.MethodPost, host+"/lazybox.bench.v1.RunnerService/"+method, bytes.NewReader(payload))
	req.Header.Set("Content-Type", contentType)
	req.Header.Set("Connect-Protocol-Version", "1")
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	var buf bytes.Buffer
	one := make([]byte, 1)
	if _, err = io.ReadFull(res.Body, one); err != nil {
		t.Fatal(err)
	}
	first = time.Since(started)
	buf.Write(one)
	if _, err = io.Copy(&buf, res.Body); err != nil {
		t.Fatal(err)
	}
	if res.StatusCode != http.StatusOK {
		t.Fatalf("%s: %s %s", method, res.Status, buf.String())
	}
	return buf.Bytes(), first, time.Since(started)
}

// TestMeasureHistoryLoad reports what opening a long conversation costs the
// Runner over Connect: the full replay WatchEvents serves, against the tail
// ReadSessionTail serves. Set LAZYBOX_MEASURE_HISTORY=1 to run it.
func TestMeasureHistoryLoad(t *testing.T) {
	if os.Getenv("LAZYBOX_MEASURE_HISTORY") == "" {
		t.Skip("set LAZYBOX_MEASURE_HISTORY=1 to measure")
	}
	state := t.TempDir()
	if err := os.MkdirAll(filepath.Join(state, "sessions"), 0o700); err != nil {
		t.Fatal(err)
	}
	longJournal(t, filepath.Join(state, "sessions", "long.jsonl"), 10000)
	info, _ := os.Stat(filepath.Join(state, "sessions", "long.jsonl"))
	opened := time.Now()
	b, err := runner.New(runner.Config{State: state, EventBudget: 32 << 20})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	t.Logf("journal: 10000 records, %d bytes; Runner start with it: %v", info.Size(), time.Since(opened))
	host := connectHost(t, b)
	for run := range 3 {
		data, first, total := connectJSON(t, host.URL, "WatchEvents", map[string]any{"sessionId": "long", "includePrevious": true}, true)
		t.Logf("run %d WatchEvents full replay: %d bytes, first byte %v, all %v", run, len(data), first, total)
	}
	if os.Getenv("LAZYBOX_MEASURE_TAIL") == "" {
		return
	}
	for run := range 3 {
		// What a window opening the conversation reads first: its latest turns.
		data, _, tail := connectJSON(t, host.URL, "ReadSessionTail", map[string]any{"sessionId": "long", "turns": 8, "maxBytes": 512 << 10}, false)
		var page struct {
			Events []json.RawMessage `json:"events"`
			Start  string            `json:"start"`
		}
		if err = json.Unmarshal(data, &page); err != nil {
			t.Fatal(err)
		}
		t.Logf("run %d ReadSessionTail latest 8 turns: %d records from %s, %d bytes in %v", run, len(page.Events), page.Start, len(data), tail)
		// Then the rest, in pages of about a mebibyte of journal.
		started, bytes, pages := time.Now(), 0, 0
		for before := page.Start; before != "1"; pages++ {
			data, _, _ = connectJSON(t, host.URL, "ReadSessionTail", map[string]any{"sessionId": "long", "before": before, "maxBytes": 1 << 20}, false)
			bytes += len(data)
			if err = json.Unmarshal(data, &page); err != nil {
				t.Fatal(err)
			}
			before = page.Start
		}
		t.Logf("run %d earlier history: %d pages, %d bytes in %v", run, pages, bytes, time.Since(started))
	}
}
