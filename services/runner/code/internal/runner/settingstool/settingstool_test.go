package settingstool

import (
	"encoding/json"
	"errors"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/preferences"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
)

func open(t *testing.T) *preferences.Store {
	t.Helper()
	store, err := preferences.Open(filepath.Join(t.TempDir(), "settings.json"), telemetry.RateCard{})
	if err != nil {
		t.Fatal(err)
	}
	return store
}

func proposal(revision uint64, changes map[string]any) json.RawMessage {
	data, _ := json.Marshal(map[string]any{"revision": revision, "reason": "Asked for by the user.", "changes": changes})
	return data
}

func TestGetReturnsOnlyTheEditableSettings(t *testing.T) {
	store := open(t)
	if err := store.SetRate(1, "claude/fixture-model", telemetry.Rate{Input: 1, Output: 2}, false); err != nil {
		t.Fatal(err)
	}
	out, err := Handle(store, agent.SettingsReadTool, nil)
	if err != nil {
		t.Fatal(err)
	}
	var got map[string]any
	if err = json.Unmarshal([]byte(out), &got); err != nil {
		t.Fatal(err)
	}
	want := []string{"claudeContext", "codexContext", "context", "naming", "permission", "revision"}
	if len(got) != len(want) {
		t.Fatalf("read exposes %v", got)
	}
	for _, key := range want {
		if _, ok := got[key]; !ok {
			t.Fatalf("read lacks %s: %v", key, got)
		}
	}
	if got["revision"].(float64) != 2 || got["context"] != preferences.DefaultContext {
		t.Fatalf("read: %v", got)
	}
	if strings.Contains(out, "fixture-model") {
		t.Fatal("read exposes prices")
	}
}

func TestUpdateAppliesAPartialChangeAndKeepsTheRest(t *testing.T) {
	store := open(t)
	before := store.Snapshot()
	out, err := Handle(store, agent.SettingsUpdateTool, proposal(before.Revision, map[string]any{"permission": "ask", "claudeContext": "Prefer small commits."}))
	if err != nil {
		t.Fatal(err)
	}
	var result Result
	if err = json.Unmarshal([]byte(out), &result); err != nil {
		t.Fatal(err)
	}
	if !result.Applied || strings.Join(result.Changed, ",") != "claudeContext,permission" || result.Settings.Revision != before.Revision+1 {
		t.Fatalf("result: %+v", result)
	}
	after := store.Snapshot()
	if after.Permission != "ask" || after.ClaudeContext != "Prefer small commits." || after.Context != before.Context || after.Naming != before.Naming || after.CodexContext != before.CodexContext {
		t.Fatalf("saved: %+v", after)
	}
	// Clearing a field is a change too.
	if _, err = Update(store, proposal(after.Revision, map[string]any{"permission": ""})); err != nil || store.Snapshot().Permission != "" {
		t.Fatalf("clear permission: %v %+v", err, store.Snapshot())
	}
}

func TestUpdateRefusesAStaleRevision(t *testing.T) {
	store := open(t)
	stale := store.Snapshot().Revision
	d := store.Snapshot()
	d.Naming = "generated"
	if err := store.Save(d); err != nil {
		t.Fatal(err)
	}
	_, err := Update(store, proposal(stale, map[string]any{"permission": "yolo"}))
	if !errors.Is(err, preferences.ErrConflict) || !strings.Contains(err.Error(), agent.SettingsReadTool) {
		t.Fatalf("stale revision: %v", err)
	}
	if got := store.Snapshot(); got.Permission != "" || got.Naming != "generated" {
		t.Fatalf("stale update overwrote: %+v", got)
	}
	// A call delivered twice applies once: the second names the old revision.
	current := store.Snapshot().Revision
	args := proposal(current, map[string]any{"permission": "ask"})
	if _, err = Update(store, args); err != nil {
		t.Fatal(err)
	}
	if _, err = Update(store, args); !errors.Is(err, preferences.ErrConflict) {
		t.Fatalf("replayed update: %v", err)
	}
}

func TestUpdateValidatesLikeTheSettingsPage(t *testing.T) {
	store := open(t)
	revision := store.Snapshot().Revision
	cases := map[string]json.RawMessage{
		"naming mode": proposal(revision, map[string]any{"naming": "random"}),
		"permission":  proposal(revision, map[string]any{"permission": "admin"}),
		"inherit is a session choice, not a default": proposal(revision, map[string]any{"permission": "inherit"}),
		"shared context limit":                       proposal(revision, map[string]any{"context": strings.Repeat("x", 32769)}),
		"provider context limit":                     proposal(revision, map[string]any{"codexContext": strings.Repeat("é", 16385)}),
		"non-string value":                           proposal(revision, map[string]any{"naming": 1}),
		"no revision":                                proposal(0, map[string]any{"naming": "generated"}),
		"no changes":                                 proposal(revision, map[string]any{}),
		"no-op":                                      proposal(revision, map[string]any{"naming": "repositories"}),
		"no reason":                                  json.RawMessage(`{"revision":1,"changes":{"naming":"generated"}}`),
		"top-level field":                            json.RawMessage(`{"revision":1,"reason":"r","permission":"yolo","changes":{}}`),
		"not JSON":                                   json.RawMessage(`naming=generated`),
	}
	for name, args := range cases {
		if _, err := Update(store, args); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
	// Within each field's limit, the stored total is still bounded.
	d := store.Snapshot()
	d.ClaudeContext = strings.Repeat("é", 16000)
	d.CodexContext = strings.Repeat("é", 16000)
	if err := store.Save(d); err != nil {
		t.Fatal(err)
	}
	if _, err := Update(store, proposal(store.Snapshot().Revision, map[string]any{"context": strings.Repeat("é", 32000)})); err == nil {
		t.Error("total context over 64 KiB accepted")
	}
	if got := store.Snapshot(); got.Revision != d.Revision+1 || got.Naming != "repositories" || got.Permission != "" || got.Context != preferences.DefaultContext {
		t.Fatalf("a refused update changed settings: %+v", got)
	}
}

func TestUpdateChangesOnlyAllowlistedSettings(t *testing.T) {
	store := open(t)
	revision := store.Snapshot().Revision
	for _, key := range []string{"prices", "revision", "profiles", "token", "snippets", "Permission"} {
		_, err := Update(store, proposal(revision, map[string]any{key: "x", "naming": "generated"}))
		if err == nil || !strings.Contains(err.Error(), "cannot be changed") {
			t.Fatalf("%s: %v", key, err)
		}
	}
	if store.Snapshot().Revision != revision {
		t.Fatal("a refused proposal saved part of its changes")
	}
	if _, err := Handle(store, "lazybox_command", nil); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("other tool: %v", err)
	}
	if _, err := Handle(nil, agent.SettingsReadTool, nil); err == nil {
		t.Fatal("no store")
	}
}
