package telemetry

import (
	"context"
	"encoding/json"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func scope() Scope {
	return Scope{Workspace: "workspace", Conversation: "conversation", Execution: "execution", Profile: "account", Provider: "claude", Attempt: "attempt"}
}
func store(t *testing.T) *SQLite {
	t.Helper()
	s, e := Open(filepath.Join(t.TempDir(), "usage.db"))
	if e != nil {
		t.Fatal(e)
	}
	t.Cleanup(func() { s.Close() })
	return s
}
func read(t *testing.T, s Store) Report {
	t.Helper()
	r, e := s.Report(context.Background(), Query{Workspace: "workspace"})
	if e != nil {
		t.Fatal(e)
	}
	return r
}

const claudeStream = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":100,\"output_tokens\":0,\"cache_read_input_tokens\":200,\"cache_creation_input_tokens\":30,\"cache_creation\":{\"ephemeral_1h_input_tokens\":10}}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":12}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\ndata: {\"type\":\"message_stop\"}\n\n"

func TestParserChunkBoundariesAndCacheBuckets(t *testing.T) {
	for _, newline := range []string{"\n", "\r\n", "\r"} {
		wire := strings.ReplaceAll(claudeStream, "\n", newline)
		for chunk := 1; chunk < 80; chunk++ {
			p := NewParser("claude", "text/event-stream")
			for i := 0; i < len(wire); i += chunk {
				end := min(i+chunk, len(wire))
				p.Push([]byte(wire[i:end]))
			}
			r := p.Finish(true)
			if !p.Terminal() || r.Status != "complete" || value(r.Tokens.Input) != 100 || value(r.Tokens.Output) != 20 || value(r.Tokens.CacheRead) != 200 || value(r.Tokens.CacheWrite5m) != 20 || value(r.Tokens.CacheWrite1h) != 10 {
				t.Fatalf("chunk %d: %+v", chunk, r)
			}
			r.Scope = scope()
			r = DefaultRates().Price(r)
			if r.Nanos == nil || *r.Nanos != 795000 {
				t.Fatalf("incorrect price %+v", r)
			}
		}
	}
}
func TestResponsesOnlyFinalUsageAndUnknownPrices(t *testing.T) {
	initial := `data: {"type":"response.created","response":{"id":"r","model":"gpt-5.4","usage":{"input_tokens":900,"output_tokens":99}}}` + "\n\n"
	p := NewParser("codex", "text/event-stream")
	p.Push([]byte(initial))
	if r := p.Finish(true); r.Status == "complete" || r.Tokens.Input != nil {
		t.Fatal("provisional usage counted")
	}
	p.Push([]byte(`data: {"type":"response.completed","response":{"id":"r","model":"gpt-5.4","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80},"output_tokens":10,"output_tokens_details":{"reasoning_tokens":7}}}}` + "\n\n"))
	r := p.Finish(false)
	r.Scope = scope()
	r.Scope.Provider = "codex"
	r = DefaultRates().Price(r)
	if r.Status != "complete" || value(r.Tokens.Input) != 20 || value(r.Tokens.Output) != 10 || value(r.Nanos) != 220000 {
		t.Fatalf("%+v", r)
	}
	r.Model = "unknown"
	if DefaultRates().Price(r).Nanos != nil {
		t.Fatal("unknown model priced")
	}
	r.Model = "gpt-5.4"
	r.Tier = "priority"
	if DefaultRates().Price(r).Nanos != nil {
		t.Fatal("unknown tier priced")
	}
	p = NewParser("codex", "text/event-stream")
	p.Push([]byte(`data: {"type":"response.completed","response":{"id":"r"}}` + "\n\n"))
	if p.Finish(true).Status == "complete" {
		t.Fatal("missing usage became complete")
	}
	p = NewParser("claude", "application/json")
	p.Push([]byte(strings.Repeat("x", 3<<20)))
	if p.Finish(true).Tokens.Input != nil {
		t.Fatal("oversized payload counted")
	}
}
func TestLedgerRestartDeduplicationAndAccountRollup(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage.db")
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	r := Record{ID: "request", Scope: scope(), Status: "complete", Source: "gateway", ResponseID: "same", Model: "known", Nanos: number(100), Tokens: Tokens{Input: number(2)}}
	for _, id := range []string{"request", "retry"} {
		r.ID = id
		if err = s.Put(context.Background(), r); err != nil {
			t.Fatal(err)
		}
	}
	r.ID = "second-account"
	r.Scope.Profile = "second"
	if err = s.Put(context.Background(), r); err != nil {
		t.Fatal(err)
	}
	r.ID = "unfinished"
	r.Status = "pending"
	r.Nanos = nil
	r.Tokens = Tokens{}
	if err = s.Put(context.Background(), r); err != nil {
		t.Fatal(err)
	}
	s.Close()
	s, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	report := read(t, s)
	var total int64
	var incomplete int64
	for _, g := range report.Groups {
		total += value(g.Nanos)
		incomplete += g.Incomplete
	}
	if total != 200 || incomplete != 1 {
		t.Fatalf("total=%d incomplete=%d", total, incomplete)
	}
	for _, r := range report.Recent {
		if r.ID == "unfinished" && r.Status != "interrupted" {
			t.Fatal("pending request not recovered")
		}
	}
	data, _ := os.ReadFile(path)
	if strings.Contains(string(data), "Bearer") {
		t.Fatal("unexpected credential")
	}
}
func TestCumulativeResumeDoesNotDoubleCount(t *testing.T) {
	s := store(t)
	r := Record{Scope: scope(), ResponseID: "thread", Source: "sdk", Status: "complete", Tokens: Tokens{Input: number(10), Output: number(5), CacheRead: number(0)}}
	r.Scope.Provider = "codex"
	for i := 0; i < 2; i++ {
		if err := s.PutCumulative(context.Background(), r, false); err != nil {
			t.Fatal(err)
		}
	}
	r.Scope.Execution = "resumed"
	r.Tokens.Input = number(15)
	r.Tokens.Output = number(8)
	if err := s.PutCumulative(context.Background(), r, true); err != nil {
		t.Fatal(err)
	}
	var input, output int64
	for _, g := range read(t, s).Groups {
		input += value(g.Tokens.Input)
		output += value(g.Tokens.Output)
	}
	if input != 15 || output != 8 {
		t.Fatalf("%d %d", input, output)
	}
	r.ResponseID = "imported"
	if err := s.PutCumulative(context.Background(), r, true); err != nil {
		t.Fatal(err)
	}
	r.Tokens.Input = number(20)
	if err := s.PutCumulative(context.Background(), r, true); err != nil {
		t.Fatal(err)
	}
	input = 0
	for _, g := range read(t, s).Groups {
		input += value(g.Tokens.Input)
	}
	if input != 20 {
		t.Fatal(input)
	}
}
func TestGatewayPreservesWireAuthAndCommitsBeforeTerminalDelivery(t *testing.T) {
	s := store(t)
	var gotAuth, gotBody string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		body, _ := io.ReadAll(r.Body)
		gotBody = string(body)
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, claudeStream)
	}))
	defer upstream.Close()
	g, err := StartGateway(s, DefaultRates(), Upstreams{Claude: upstream.URL})
	if err != nil {
		t.Fatal(err)
	}
	defer g.Close()
	base, revoke, err := g.Register(scope())
	if err != nil {
		t.Fatal(err)
	}
	request, _ := http.NewRequest("POST", base+"/v1/messages?beta=true", strings.NewReader(`{"messages":["private prompt"]}`))
	request.Header.Set("Authorization", "Bearer fixture-secret")
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	wire, _ := io.ReadAll(response.Body)
	response.Body.Close()
	if string(wire) != claudeStream || gotAuth != "Bearer fixture-secret" || gotBody != `{"messages":["private prompt"]}` {
		t.Fatal("forwarding changed auth or payload")
	}
	report := read(t, s)
	if len(report.Groups) != 1 || value(report.Groups[0].Nanos) != 795000 || report.Groups[0].Incomplete != 0 {
		t.Fatalf("%+v", report)
	}
	raw, _ := json.Marshal(report)
	if strings.Contains(string(raw), "private prompt") || strings.Contains(string(raw), "fixture-secret") {
		t.Fatal("private content leaked")
	}
	revoke()
	response, err = http.Post(base+"/v1/messages", "application/json", strings.NewReader("{}"))
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != 404 {
		t.Fatal("revoked route accepted")
	}
}
func TestGatewayChatGPTSplitAndRedirectDoesNotLeakCredentials(t *testing.T) {
	s := store(t)
	hits := 0
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		if r.URL.Path != "/responses" || r.Header.Get("Chatgpt-Account-Id") != "saved-account" {
			t.Error("wrong route")
		}
		w.Header().Set("Location", "https://example.invalid")
		w.WriteHeader(307)
	}))
	defer up.Close()
	g, err := StartGateway(s, DefaultRates(), Upstreams{ChatGPT: up.URL, OpenAI: "http://127.0.0.1:1"})
	if err != nil {
		t.Fatal(err)
	}
	defer g.Close()
	sc := scope()
	sc.Provider = "codex"
	base, _, _ := g.Register(sc)
	req, _ := http.NewRequest("POST", base+"/responses", strings.NewReader("{}"))
	req.Header.Set("Chatgpt-Account-Id", "saved-account")
	client := http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	r, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	r.Body.Close()
	if hits != 1 || r.StatusCode != 502 {
		t.Fatal("redirect followed")
	}
}
func TestAdapterScopeContextAndMeterAuthority(t *testing.T) {
	s := store(t)
	g, err := StartGateway(s, DefaultRates(), Upstreams{})
	if err != nil {
		t.Fatal(err)
	}
	defer g.Close()
	native := &agenttest.Adapter{}
	a := Adapter{Native: native, Store: s, Prices: DefaultRates(), Gateway: g, Scope: scope()}
	o := agent.Open{ID: "execution", Profile: "second", Permission: "", Env: []string{"ANTHROPIC_AUTH_TOKEN=private"}, Gateway: &agent.Gateway{ID: "metered"}}
	session, err := a.Open(context.Background(), o)
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	if native.Session.Open.Profile != "second" || native.Session.Open.Permission != "" || native.Session.Open.Env[0] != "ANTHROPIC_AUTH_TOKEN=private" {
		t.Fatal("account or permissions changed")
	}
	for _, ev := range []agent.Event{{Type: "usage", Data: json.RawMessage(`{"requestId":"msg","model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":20}}`)}, {Type: "context", AgentID: "child", Data: json.RawMessage(`{"used":120,"capacity":1000}`)}} {
		native.Session.Events <- ev
		if _, err = session.Receive(context.Background()); err != nil {
			t.Fatal(err)
		}
	}
	report := read(t, s)
	if len(report.Groups) != 0 || len(report.Contexts) != 1 || report.Contexts[0].Scope.Profile != "second" || report.Contexts[0].Agent != "child" {
		t.Fatalf("%+v", report)
	}
	o.Gateway = nil
	session2, err := a.Open(context.Background(), o)
	if err != nil {
		t.Fatal(err)
	}
	defer session2.Close()
	native.Session.Events <- agent.Event{Type: "usage", Data: json.RawMessage(`{"requestId":"msg","model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":20}}`)}
	if _, err = session2.Receive(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(read(t, s).Groups) != 1 {
		t.Fatal("SDK fallback missing")
	}
}

func TestMissingRateCanBeAppliedToHistoryWithoutRepricingCapturedCosts(t *testing.T) {
	ctx := context.Background()
	s := store(t)
	base := Record{Scope: scope(), Source: "provider", Model: "claude-fable-5-1", Status: "complete", Tokens: Tokens{Input: number(2), CacheRead: number(12857), CacheWrite5m: number(12155), CacheWrite1h: number(0), Output: number(4)}}
	priced := DefaultRates().Price(base)
	if priced.Nanos == nil || *priced.Nanos != 155371750 {
		t.Fatalf("wrong Fable 5.1 estimate: %+v", priced)
	}
	unpriced := base
	unpriced.ID = "missing"
	unpriced.ResponseID = "one"
	unpriced.Reason = "No exact model price"
	captured := priced
	captured.ID = "captured"
	captured.ResponseID = "two"
	incomplete := base
	incomplete.ID = "incomplete"
	incomplete.ResponseID = "three"
	incomplete.Reason = "Missing cache TTL"
	for _, r := range []Record{unpriced, captured, incomplete} {
		if err := s.Put(ctx, r); err != nil {
			t.Fatal(err)
		}
	}
	rates := DefaultRates()
	rates["claude/claude-fable-5-1"] = Rate{Version: "override", Input: 1, Output: 1, CacheRead: 1, CacheWrite5m: 1, CacheWrite1h: 1}
	report, err := s.Report(ctx, Query{Workspace: "workspace", Prices: rates})
	if err != nil {
		t.Fatal(err)
	}
	found := map[string]Record{}
	for _, r := range report.Recent {
		found[r.ID] = r
	}
	if found["missing"].Nanos == nil || *found["missing"].Nanos != 25018 || found["missing"].PriceVersion != "override" {
		t.Fatal(found)
	}
	if found["captured"].Nanos == nil || *found["captured"].Nanos != *priced.Nanos {
		t.Fatal("historical price rewritten", found)
	}
	if found["incomplete"].Nanos != nil || found["incomplete"].Reason != "Missing cache TTL" {
		t.Fatal("unknown usage was priced", found)
	}
	raw := read(t, s)
	for _, r := range raw.Recent {
		if r.ID == "missing" && r.Nanos != nil {
			t.Fatal("read rewrote immutable observation")
		}
	}
}

func TestCodexUsageReplayDoesNotInventContextCategories(t *testing.T) {
	ctx := context.Background()
	s := store(t)
	sc := scope()
	sc.Provider = "codex"
	native := &agenttest.Adapter{}
	a := Adapter{Native: native, Store: s, Prices: DefaultRates(), Scope: sc}
	session, err := a.Open(ctx, agent.Open{ID: "execution", Profile: "account"})
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	native.Session.Events <- agent.Event{Type: "usage", Data: json.RawMessage(`{"threadId":"fixture","tokenUsage":{"last":{"totalTokens":1234},"modelContextWindow":100000,"total":{"inputTokens":1000,"cachedInputTokens":200,"outputTokens":234}}}`)}
	if _, err = session.Receive(ctx); err != nil {
		t.Fatal(err)
	}
	report := read(t, s)
	if len(report.Contexts) != 1 {
		t.Fatal(report)
	}
	c := report.Contexts[0]
	if c.Used == nil || *c.Used != 1234 || c.Capacity == nil || *c.Capacity != 100000 || len(c.Categories) != 0 {
		t.Fatalf("invented or lost context: %+v", c)
	}
	if len(report.Groups) != 1 || *report.Groups[0].Tokens.Input != 800 || *report.Groups[0].Tokens.CacheRead != 200 {
		t.Fatal(report.Groups)
	}
}

func TestContextDetailsSurviveStorage(t *testing.T) {
	s := store(t)
	ctx := context.Background()
	c := ContextSnapshot{Scope: scope(), Agent: "root", Used: number(10), Categories: []Category{{Name: "system_tools", Kind: "used", Included: true, Tokens: number(10), Details: []ContextDetail{{Name: "Read", Tokens: number(10)}}}}}
	if err := s.PutContext(ctx, c); err != nil {
		t.Fatal(err)
	}
	result := read(t, s)
	if len(result.Contexts) != 1 || len(result.Contexts[0].Categories[0].Details) != 1 || result.Contexts[0].Categories[0].Details[0].Name != "Read" {
		t.Fatal(result)
	}
}

// A re-attached session keeps its metering attempt, and the native layer is
// pointed at this Runner's own gateway route rather than the departed one's.
func TestReattachedSessionKeepsItsMeteringAttemptAndGetsAFreshRoute(t *testing.T) {
	s := store(t)
	g, err := StartGateway(s, DefaultRates(), Upstreams{})
	if err != nil {
		t.Fatal(err)
	}
	defer g.Close()
	native := &agenttest.Adapter{Durable: json.RawMessage(`{"host":"h1"}`)}
	a := Adapter{Native: native, Store: s, Prices: DefaultRates(), Gateway: g, Scope: scope()}
	first, err := a.Open(context.Background(), agent.Open{ID: "execution", Profile: "second", Gateway: &agent.Gateway{ID: "metered"}})
	if err != nil {
		t.Fatal(err)
	}
	firstRoute := native.Session.Open.Gateway.URL
	state := first.(agent.Durable).DurableState()
	var recorded struct {
		Attempt string
		Native  json.RawMessage
	}
	if json.Unmarshal(state, &recorded) != nil || recorded.Attempt == "" || string(recorded.Native) != `{"host":"h1"}` {
		t.Fatalf("metering state: %s", state)
	}
	if err = first.(agent.Detacher).Detach(); err != nil {
		t.Fatal(err)
	}
	again, err := a.Open(context.Background(), agent.Open{ID: "execution", Profile: "second", Gateway: &agent.Gateway{ID: "metered"}, Host: &agent.Hosting{Root: "/hosts", Attach: &agent.Attachment{State: state}}})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if string(native.Session.Open.Host.Attach.State) != `{"host":"h1"}` {
		t.Fatal("native layer did not receive its own state")
	}
	if route := native.Session.Open.Gateway.URL; route == "" || route == firstRoute {
		t.Fatalf("re-attached session not given a live route: %q", route)
	}
	var after struct{ Attempt string }
	_ = json.Unmarshal(again.(agent.Durable).DurableState(), &after)
	if after.Attempt != recorded.Attempt {
		t.Fatal("metering attempt changed across re-attachment")
	}
}
