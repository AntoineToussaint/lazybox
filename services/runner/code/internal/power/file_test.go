package power

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

const sample = `version: 1
actions:
  - id: test-then-fix
    title: Run the tests, ask the agent to fix failures
    key: t
    where: [palette, menu, conversation]
    steps:
      - run: test
      - when: failed
        send: The tests failed; fix them.
      - when: succeeded
        open: changes
  - id: review
    steps:
      - send: { snippet: review, to: new, wait: true }
      - wait: { until: idle, timeout: 10m }
      - open: { file: api/README.md, line: 3 }
      - open: https://example.com/board
      - permission: yolo
      - command: npm test
hooks:
  - id: notify
    on: agent.needs-input
    run: [terminal-notifier, -message, needs you]
  - id: slack
    on: [turn.failed, pr.checks-failed]
    filter: { workspace: "release*", repository: acme/api }
    webhook:
      url: https://hooks.example.com/x
      headers: { Authorization: Bearer secret }
    timeout: 20s
  - id: keep-going
    on: turn.done
    grants: [respond, read]
    shell: lazybox send "$LAZYBOX_EVENT_CONVERSATION" next
    enabled: false
`

func TestParseReadsEveryStepAndHookForm(t *testing.T) {
	f, issues, err := Parse([]byte(sample))
	if err != nil || len(issues) != 0 {
		t.Fatalf("parse: %v %v", err, issues)
	}
	if len(f.Actions) != 2 || len(f.Hooks) != 3 {
		t.Fatalf("loaded %d actions, %d hooks", len(f.Actions), len(f.Hooks))
	}
	a := f.Actions[0]
	if a.Key != "t" || len(a.Steps) != 3 || a.Steps[0].Kind != StepRun || a.Steps[0].Action != "test" || a.Steps[1].When != "failed" || a.Steps[1].Text == "" || a.Steps[2].Panel != "changes" {
		t.Fatalf("first action: %+v", a)
	}
	r := f.Actions[1]
	if r.Title != "Review" || strings.Join(r.Where, ",") != "palette,menu" {
		t.Fatalf("defaults: %+v", r)
	}
	s := r.Steps
	if s[0].Snippet != "review" || s[0].To != "new" || !s[0].Wait || s[1].Until != "idle" || s[1].Timeout != 10*time.Minute ||
		s[2].File != "api/README.md" || s[2].FileLine != 3 || s[3].URL == "" || s[4].Permission != "yolo" || s[5].Command != "npm test" {
		t.Fatalf("steps: %+v", s)
	}
	h := f.Hooks[1]
	if h.Webhook != "https://hooks.example.com/x" || h.Headers["Authorization"] != "Bearer secret" || h.Timeout != 20*time.Second || len(h.On) != 2 || h.Repositories[0] != "acme/api" {
		t.Fatalf("webhook hook: %+v", h)
	}
	if f.Hooks[0].Timeout != DefaultHookTimeout || !f.Hooks[0].Enabled || f.Hooks[2].Enabled || strings.Join(f.Hooks[2].Grants, ",") != "respond,read" {
		t.Fatalf("hook defaults: %+v", f.Hooks)
	}
}

// Every mistake is reported at its line, and only the faulty entry is left out.
func TestParseLeavesOutFaultyEntriesWithTheirLine(t *testing.T) {
	cases := map[string]struct{ body, want string }{
		"unknown step":       {"actions:\n  - id: a\n    steps:\n      - jump: x\n", "unknown field \"jump\""},
		"two kinds":          {"actions:\n  - id: a\n    steps:\n      - send: x\n        open: changes\n", "does one thing"},
		"first when":         {"actions:\n  - id: a\n    steps:\n      - when: failed\n        send: x\n", "no previous step"},
		"bad panel":          {"actions:\n  - id: a\n    steps:\n      - open: kitchen\n", "neither a panel"},
		"bad url":            {"actions:\n  - id: a\n    steps:\n      - open: { url: \"ftp://x\" }\n", "not an http"},
		"escaping file":      {"actions:\n  - id: a\n    steps:\n      - open: { file: ../etc/passwd }\n", "inside the workspace"},
		"bad key":            {"actions:\n  - id: a\n    key: tt\n    steps:\n      - send: x\n", "one lowercase letter"},
		"bad id":             {"actions:\n  - id: A b\n    steps:\n      - send: x\n", "lowercase letters"},
		"no steps":           {"actions:\n  - id: a\n", "needs steps"},
		"bad permission":     {"actions:\n  - id: a\n    steps:\n      - permission: root\n", "not one of"},
		"send needs text":    {"actions:\n  - id: a\n    steps:\n      - send: { to: new }\n", "either text or a snippet"},
		"timeout too long":   {"actions:\n  - id: a\n    steps:\n      - wait: { until: idle, timeout: 48h }\n", "between 1s"},
		"hook no target":     {"hooks:\n  - id: h\n    on: turn.done\n", "exactly one of run"},
		"hook two targets":   {"hooks:\n  - id: h\n    on: turn.done\n    run: [x]\n    shell: y\n", "exactly one of run"},
		"hook bad event":     {"hooks:\n  - id: h\n    on: turn.maybe\n    run: [x]\n", "not one of"},
		"hook bad grant":     {"hooks:\n  - id: h\n    on: turn.done\n    run: [x]\n    grants: [admin]\n", "not one of"},
		"webhook grant":      {"hooks:\n  - id: h\n    on: turn.done\n    webhook: https://x.example\n    grants: [read]\n", "grants are for run and shell"},
		"hook long timeout":  {"hooks:\n  - id: h\n    on: turn.done\n    run: [x]\n    timeout: 1h\n", "between 1s"},
		"hook unknown field": {"hooks:\n  - id: h\n    on: turn.done\n    run: [x]\n    env: {A: b}\n", "unknown field \"env\""},
	}
	for name, c := range cases {
		t.Run(name, func(t *testing.T) {
			f, issues, err := Parse([]byte(c.body))
			if err != nil {
				t.Fatal(err)
			}
			if len(f.Actions)+len(f.Hooks) != 0 {
				t.Fatalf("faulty entry loaded: %+v", f)
			}
			found := false
			for _, i := range issues {
				if strings.Contains(i.Message, c.want) && i.Line > 0 && i.Severity == "error" {
					found = true
				}
			}
			if !found {
				t.Fatalf("issues %v lack %q with a line", issues, c.want)
			}
		})
	}
}

func TestParseKeepsGoodEntriesBesideBadOnes(t *testing.T) {
	body := "actions:\n  - id: good\n    key: g\n    steps:\n      - send: hi\n  - id: good\n    steps:\n      - send: again\n  - id: other\n    key: g\n    steps:\n      - send: x\n  - id: broken\n    steps: []\nextra: 1\n"
	f, issues, err := Parse([]byte(body))
	if err != nil {
		t.Fatal(err)
	}
	if len(f.Actions) != 1 || f.Actions[0].ID != "good" {
		t.Fatalf("actions: %+v", f.Actions)
	}
	text := ""
	for _, i := range issues {
		text += i.String() + "\n"
	}
	for _, want := range []string{"already has the id", "already used by \"good\"", "non-empty list", "warning: line 15 (extra): unknown section"} {
		if !strings.Contains(text, want) {
			t.Errorf("issues lack %q:\n%s", want, text)
		}
	}
}

func TestParseRefusesAFileItCannotRead(t *testing.T) {
	for name, body := range map[string]string{
		"yaml":      "actions: [\n",
		"list":      "- a\n- b\n",
		"documents": "actions: []\n---\nhooks: []\n",
		"large":     "# " + strings.Repeat("x", MaxFileBytes) + "\n",
	} {
		if _, _, err := Parse([]byte(body)); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
	for _, empty := range []string{"", "# only comments\n", "~\n"} {
		if f, issues, err := Parse([]byte(empty)); err != nil || len(issues) != 0 || len(f.Actions) != 0 {
			t.Errorf("empty %q: %v %v", empty, err, issues)
		}
	}
}

// The starter Init writes is valid and its schema is JSON.
func TestStarterAndSchema(t *testing.T) {
	f, issues, err := Parse([]byte(starter))
	if err != nil || len(issues) != 0 || len(f.Actions) == 0 {
		t.Fatalf("starter: %v %v %+v", err, issues, f)
	}
	var schema map[string]any
	if err := json.Unmarshal([]byte(Schema), &schema); err != nil {
		t.Fatal(err)
	}
	defs := schema["$defs"].(map[string]any)
	events := defs["event"].(map[string]any)["enum"].([]any)
	if len(events) != len(Events) {
		t.Fatalf("schema events %v, want %v", events, Events)
	}
	for i, e := range Events {
		if events[i] != e {
			t.Fatalf("schema events %v, want %v", events, Events)
		}
	}
	panels := defs["step"].(map[string]any)["properties"].(map[string]any)["open"].(map[string]any)["oneOf"].([]any)[1].(map[string]any)["properties"].(map[string]any)["panel"].(map[string]any)["enum"].([]any)
	if len(panels) != len(Panels) {
		t.Fatalf("schema panels %v, want %v", panels, Panels)
	}
}

func TestManagerReloadsKeepsTheLastGoodVersionAndInits(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config", "lazybox", "actions.yaml")
	m := NewManager(path)
	if s := m.Snapshot(); s.Exists || s.Err != "" || s.Revision != 1 {
		t.Fatalf("missing file: %+v", s)
	}
	if _, changed := m.Reload(); changed {
		t.Fatal("nothing changed")
	}
	snap, err := m.Init()
	if err != nil || !snap.Exists || len(snap.File.Actions) == 0 {
		t.Fatalf("init: %v %+v", err, snap)
	}
	if info, err := os.Stat(path); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("file mode: %v %v", info, err)
	}
	if _, err := os.Stat(m.SchemaPath()); err != nil {
		t.Fatal("schema not written", err)
	}
	// Init never replaces the person's file.
	changed := m.Changed()
	if err := os.WriteFile(path, []byte("actions:\n  - id: mine\n    steps:\n      - send: hi\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	good, err := m.Init()
	if err != nil || len(good.File.Actions) != 1 || good.File.Actions[0].ID != "mine" {
		t.Fatalf("reload: %+v", good)
	}
	select {
	case <-changed:
	default:
		t.Fatal("Changed not signalled")
	}
	// A broken save keeps the last good actions, marked stale.
	if err := os.WriteFile(path, []byte("actions: [\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	broken, _ := m.Reload()
	if broken.Err == "" || !broken.Stale || len(broken.File.Actions) != 1 || broken.Revision <= good.Revision {
		t.Fatalf("broken: %+v", broken)
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	gone, _ := m.Reload()
	if gone.Exists || gone.Stale || len(gone.File.Actions) != 0 {
		t.Fatalf("removed: %+v", gone)
	}
}

func TestManagerWatchPicksUpChanges(t *testing.T) {
	path := filepath.Join(t.TempDir(), "actions.yaml")
	m := NewManager(path)
	stop := make(chan struct{})
	defer close(stop)
	go m.Watch(stop, 10*time.Millisecond)
	changed := m.Changed()
	if err := os.WriteFile(path, []byte("actions:\n  - id: later\n    steps:\n      - send: hi\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	select {
	case <-changed:
	case <-time.After(5 * time.Second):
		t.Fatal("no reload")
	}
	if got := m.Snapshot().File.Actions; len(got) != 1 || got[0].ID != "later" {
		t.Fatalf("watched: %+v", got)
	}
}
