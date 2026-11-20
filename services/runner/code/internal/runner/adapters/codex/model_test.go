package codex

import (
	"context"
	"encoding/json"
	"os"
	"reflect"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A model switch applies from the next turn: turn/start carries it, plan
// mode's collaboration settings name it, and a handoff keeps it.
func TestLiveModelSwitchAppliesToFollowingTurns(t *testing.T) {
	s := &session{settings: []byte(`{"model":"gpt-old","approvalPolicy":"never"}`)}
	if !s.Capabilities().LiveModel {
		t.Fatal("Codex turns accept a model")
	}
	r, err := s.Apply(context.Background(), agent.Operation{ID: "m", Kind: agent.Model, Model: "gpt-new", Effort: "high"})
	if err != nil || r.Delivery != "accepted" {
		t.Fatalf("switch refused: %+v %v", r, err)
	}
	if string(s.overrides["model"]) != `"gpt-new"` || string(s.overrides["effort"]) != `"high"` || s.model != "gpt-new" {
		t.Fatalf("model not kept for turn/start: %s %q", s.overrides, s.model)
	}
	if got := string(s.SessionSettings()); got != `{"approvalPolicy":"never","effort":"high","model":"gpt-new"}` {
		t.Fatalf("a handoff would carry the old model: %s", got)
	}
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "e", Kind: agent.Model}); err == nil {
		t.Fatal("an empty model was accepted")
	}
}

// A thread on Codex's default model still says which model that is: once at
// start (session_ready, as the Claude worker does) and on every usage of the
// main agent, so the picker marks it and metering reports it. A switch is
// what usage then names.
func TestSessionSaysTheModelItRunsOn(t *testing.T) {
	t.Setenv("FIXTURE_COMPACTION", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: fixtureRoot(t)}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	ready := receive(t, s, "session_ready")
	if string(ready.Data) != `{"model":"fixture-model"}` || ready.AgentID != "root" {
		t.Fatalf("ready: %+v %s", ready, ready.Data)
	}
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "m", Kind: agent.Model, Model: "gpt-6-luna"}); err != nil {
		t.Fatal(err)
	}
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "work"}); err != nil {
		t.Fatal(err)
	}
	usage := receive(t, s, "usage")
	var data struct {
		Model      string          `json:"model"`
		TokenUsage json.RawMessage `json:"tokenUsage"`
	}
	if err = json.Unmarshal(usage.Data, &data); err != nil || data.Model != "gpt-6-luna" || len(data.TokenUsage) == 0 {
		t.Fatalf("usage: %s %v", usage.Data, err)
	}
	if _, ok := readyEvent(""); ok {
		t.Fatal("no model is nothing to say")
	}
	if got := string(withModel(json.RawMessage(`{"model":"theirs"}`), "ours")); got != `{"model":"theirs"}` {
		t.Fatalf("a model Codex named was replaced: %s", got)
	}
	if got := string(withModel(json.RawMessage(`not json`), "ours")); got != `not json` {
		t.Fatalf("unreadable params changed: %s", got)
	}
}

// A session lists the models its own app-server offers (its subscription's),
// every page, without the hidden ones or repeats, once, as a command_catalog.
func TestSessionReportsItsModelsForThePicker(t *testing.T) {
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	ev := receive(t, s, "command_catalog")
	var catalog struct{ Models []agent.ModelChoice }
	if err = json.Unmarshal(ev.Data, &catalog); err != nil || ev.AgentID != "root" {
		t.Fatalf("catalog: %s %v", ev.Data, err)
	}
	want := []agent.ModelChoice{{Value: "gpt-6.1-sol", DisplayName: "GPT-6.1-Sol", Description: "Workhorse", EffortLevels: []string{"high"}}, {Value: "gpt-6-luna", DisplayName: "GPT-6-Luna", Description: "Fast"}, {Value: "gpt-6-astra", DisplayName: "gpt-6-astra", Description: "Frontier"}}
	if !reflect.DeepEqual(catalog.Models, want) {
		t.Fatalf("models %+v, want %+v", catalog.Models, want)
	}
}

// A Codex without model/list still opens: /model falls back to the Runner's list.
func TestSessionOpensWhenCodexCannotListModels(t *testing.T) {
	t.Setenv("FIXTURE_NO_MODEL_LIST", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if _, err = listModels(context.Background(), s.(*session).p); err == nil {
		t.Fatal("an unknown method listed models")
	}
	if _, ok := catalogEvent(nil); ok {
		t.Fatal("no models is nothing to say")
	}
}

// The Runner's own listing: an app-server started only for it, then stopped.
func TestListModelsStartsAnAppServerOnlyToList(t *testing.T) {
	self, _ := os.Executable()
	models, err := ListModels(context.Background(), self, os.Environ(), t.TempDir())
	if err != nil || len(models) != 3 || models[0].Value != "gpt-6.1-sol" {
		t.Fatalf("models %+v %v", models, err)
	}
	t.Setenv("FIXTURE_NO_MODEL_LIST", "1")
	if _, err = ListModels(context.Background(), self, os.Environ(), t.TempDir()); err == nil {
		t.Fatal("a failed listing reported models")
	}
}

func TestListMcpServersUsesTheCurrentThread(t *testing.T) {
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	receipt, err := s.Apply(context.Background(), agent.Operation{ID: "inventory", Kind: agent.ListMcpServers})
	if err != nil || receipt.Delivery != "acknowledged" {
		t.Fatalf("inventory request failed: %+v %v", receipt, err)
	}
	event := receive(t, s, "mcp_servers")
	if event.OperationID != "inventory" || event.Text != "MCP servers: fixture (2 tools) · private (sign-in required)" {
		t.Fatalf("unexpected inventory: %+v", event)
	}
}
