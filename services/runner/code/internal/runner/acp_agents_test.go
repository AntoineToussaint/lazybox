package runner

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/acp"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
)

// fakeACPAgent is the fake ACP agent (scripts/fixtures/acp-fake.mjs) as a
// custom agent the person added.
func fakeACPAgent(t *testing.T) acp.Agent {
	t.Helper()
	node, err := exec.LookPath("node")
	if err != nil {
		t.Skip("node is not on the PATH; the fake ACP agent needs it")
	}
	script, err := filepath.Abs("../../../../../scripts/fixtures/acp-fake.mjs")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(script); err != nil {
		t.Fatalf("fake ACP agent missing: %v", err)
	}
	return acp.Agent{ID: "fake", Title: "Fake agent", Command: node, Args: []string{script}}
}

// pendingRequest waits for the execution's one pending request.
func pendingRequest(t *testing.T, e *Execution) string {
	t.Helper()
	for end := time.Now().Add(15 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
		for id := range e.Engine.Snapshot().Pending {
			return id
		}
	}
	t.Fatalf("no request arrived: %+v", e.Engine.Snapshot())
	return ""
}

// An ACP agent the person added runs as a conversation through the whole
// Runner: its profile, the session host, the journal, approvals, files
// written in the workspace, its terminal as a managed command in the
// console, and its context and cost in telemetry.
func TestACPAgentRunsAsAConversation(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if _, err = b.SaveACPAgent(fakeACPAgent(t)); err != nil {
		t.Fatal(err)
	}
	var profile accounts.Profile
	for _, p := range b.Profiles() {
		if p.Provider == "acp:fake" {
			profile = p
		}
	}
	if profile.ID != "acp-fake" || profile.Label != "Fake agent" || profile.Isolation != "inherited" {
		t.Fatalf("profile: %+v", profile)
	}
	e, err := b.OpenEmptyWorkspace("acp-fake", accounts.Options{Permission: "ask"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(e.Cwd, "README.md"), []byte("fixture readme\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if hosted, ok := e.Engine.(agent.Hosted); !ok || hosted.HostID() == "" {
		t.Fatal("the ACP agent does not run in a session host")
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "tour", Kind: agent.Message, Text: "tour"}); err != nil {
		t.Fatal(err)
	}
	request := pendingRequest(t, e)
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: request, Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	events := awaitEvent(t, b, e.ID, "turn_ended")
	var message strings.Builder
	tools := map[string]string{}
	for _, ev := range events {
		switch ev.Type {
		case "message_delta":
			message.WriteString(ev.Text)
		case "tool_activity":
			var call struct{ ID, Name, Status string }
			_ = json.Unmarshal(ev.Data, &call)
			tools[call.ID] = call.Name + ":" + call.Status
		}
	}
	if message.String() != "Done: read the README, wrote notes.txt and ran the check." {
		t.Fatalf("message: %q", message.String())
	}
	if tools["call-read"] != "Read:completed" || tools["call-edit"] != "Write:completed" || tools["call-run"] != "Bash:completed" || tools["plan:turn-1"] != "update_plan:completed" {
		t.Fatalf("tool lines: %v", tools)
	}
	if data, err := os.ReadFile(filepath.Join(e.Cwd, "notes.txt")); err != nil || string(data) != "first line\nsecond line\n" {
		t.Fatalf("notes.txt: %q %v", data, err)
	}
	// The agent's terminal is a managed command in the conversation's console.
	var found bool
	for _, status := range b.Commands.List(e.ID) {
		if status.Spec.Title == "echo fake-tour-check" {
			found = true
			if !status.Done || status.ExitCode == nil || *status.ExitCode != 0 {
				t.Fatalf("command: %+v", status)
			}
			c, err := b.Commands.Get(status.ID)
			if err != nil {
				t.Fatal(err)
			}
			out, _, _ := c.Read(context.Background(), 0, 1024)
			if string(out) != "fake-tour-check\n" {
				t.Fatalf("command output: %q", out)
			}
		}
	}
	if !found {
		t.Fatal("the agent's terminal is not in the console")
	}
	report, err := b.Telemetry.Report(context.Background(), telemetry.Query{Execution: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	var nanos int64
	for _, g := range report.Groups {
		if g.Nanos != nil {
			nanos += *g.Nanos
		}
	}
	if nanos != 12_300_000 {
		t.Fatalf("reported cost: %d nanos, %+v", nanos, report.Groups)
	}
	if len(report.Contexts) != 1 || report.Contexts[0].Used == nil || *report.Contexts[0].Used != 1200 || *report.Contexts[0].Capacity != 200000 {
		t.Fatalf("context: %+v", report.Contexts)
	}
}

// A read-only conversation's agent cannot start commands through Lazybox.
func TestReadOnlyACPAgentRunsNoCommand(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if _, err = b.SaveACPAgent(fakeACPAgent(t)); err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenEmptyWorkspace("acp-fake", accounts.Options{Permission: "read-only"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "run", Kind: agent.Message, Text: "run:touch planted"}); err != nil {
		t.Fatal(err)
	}
	events := awaitEvent(t, b, e.ID, "turn_ended")
	var message string
	for _, ev := range events {
		if ev.Type == "message_delta" {
			message += ev.Text
		}
	}
	if !strings.Contains(message, "read-only") {
		t.Fatalf("read-only ran a command: %q", message)
	}
	if len(b.Commands.List(e.ID)) != 0 {
		t.Fatal("a command started in a read-only conversation")
	}
}

// Built-in agents are offered when they are installed, and only then.
func TestBuiltinACPAgentsAreOfferedWhenInstalled(t *testing.T) {
	bin := t.TempDir()
	gemini := filepath.Join(bin, "gemini")
	if err := os.WriteFile(gemini, []byte("#!/bin/sh\necho 0.68.0\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	lookPath := func(name string) (string, error) {
		if name == "gemini" {
			return gemini, nil
		}
		return "", errors.New("not found")
	}
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), ACPLookPath: lookPath})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	providers := map[string]bool{}
	for _, p := range b.Profiles() {
		providers[p.Provider] = true
	}
	if !providers["acp:gemini"] || providers["acp:goose"] {
		t.Fatalf("profiles: %v", providers)
	}
	report, err := b.ProviderAvailability(context.Background(), true)
	if err != nil {
		t.Fatal(err)
	}
	states := map[string]string{}
	for _, p := range report.Providers {
		states[p.Provider] = p.State
		if p.Provider == "acp:gemini" && (p.Program.Version != "0.68.0" || p.ACP == nil || p.ACP.Icon != "agent-gemini") {
			t.Fatalf("gemini: %+v %+v", p.Program, p.ACP)
		}
		if p.Provider == "acp:goose" && p.InstallCommand == "" {
			t.Fatal("a missing agent says how to install it")
		}
	}
	if states["acp:gemini"] != "ready" || states["acp:goose"] != "not_installed" {
		t.Fatalf("states: %v", states)
	}
	// A changed built-in keeps its icon; removing the change restores it.
	if _, err = b.SaveACPAgent(acp.Agent{ID: "gemini", Title: "Gemini (preview)", Command: gemini, Args: []string{"--acp", "--model", "x"}}); err != nil {
		t.Fatal(err)
	}
	if a, _ := b.ACP.Get("gemini"); !a.Custom || a.Icon != "agent-gemini" || a.Title != "Gemini (preview)" {
		t.Fatalf("changed built-in: %+v", a)
	}
	if err = b.RemoveACPAgent("gemini"); err != nil {
		t.Fatal(err)
	}
	if a, _ := b.ACP.Get("gemini"); a.Custom || a.Title != "Gemini CLI" {
		t.Fatalf("restored built-in: %+v", a)
	}
	if err = b.RemoveACPAgent("gemini"); err == nil {
		t.Fatal("removed a built-in agent")
	}
}

// The ACP UI suite (scripts/test-acp.mjs) replays what this adapter really
// journals for the fake agent's tour and a pending permission request
// (scripts/fixtures/acp-journal.json). This test fails when the adapter's
// journal changes, until the fixture is regenerated with
// LAZYBOX_UPDATE_ACP_JOURNAL=1, so the UI is always tested on real output.
func TestACPJournalFixtureIsCurrent(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if _, err = b.SaveACPAgent(fakeACPAgent(t)); err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenEmptyWorkspace("acp-fake", accounts.Options{Permission: "ask"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	_ = os.WriteFile(filepath.Join(e.Cwd, "README.md"), []byte("# Fixture\nhello readme\n"), 0o644)
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "tour", Kind: agent.Message, Text: "tour"}); err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: pendingRequest(t, e), Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	awaitEvent(t, b, e.ID, "turn_ended")
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "ask", Kind: agent.Message, Text: "ask"}); err != nil {
		t.Fatal(err)
	}
	pendingRequest(t, e)
	var commandID string
	for _, c := range b.Commands.List(e.ID) {
		commandID = c.ID
	}
	normalize := func(raw []byte) []byte {
		text := strings.ReplaceAll(string(raw), e.Cwd, "/fixture/acp-workspace")
		if commandID != "" {
			text = strings.ReplaceAll(text, commandID, "acp-command-1")
		}
		return []byte(text)
	}
	clean := func(raw []byte) (map[string]any, string) {
		var ev map[string]any
		if json.Unmarshal(raw, &ev) != nil {
			return nil, ""
		}
		delete(ev, "transportCursor")
		delete(ev, "transportPartial")
		kind, _ := ev["type"].(string)
		return ev, kind
	}
	var catalogs, events []json.RawMessage
	for cursor := uint64(0); ; {
		ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
		r, err := b.Timeline(ctx, e.ID, cursor)
		cancel()
		if err != nil {
			break
		}
		cursor = r.Sequence
		ev, kind := clean(r.Value)
		switch kind {
		case "", "receipt", "execution_context", "session_opened", "request_resolved":
			continue
		}
		data, _ := json.Marshal(ev)
		if kind == "command_catalog" {
			catalogs = append(catalogs, normalize(data))
		} else {
			events = append(events, normalize(data))
		}
	}
	// The two catalogs (commands, models) arrive in either order.
	sort.Slice(catalogs, func(i, j int) bool { return string(catalogs[i]) < string(catalogs[j]) })
	pending := []json.RawMessage{}
	for _, p := range e.Engine.Snapshot().Pending {
		raw, _ := json.Marshal(p)
		ev, _ := clean(raw)
		data, _ := json.Marshal(ev)
		pending = append(pending, normalize(data))
	}
	got, _ := json.MarshalIndent(map[string]any{"_about": "Generated by TestACPJournalFixtureIsCurrent (internal/runner/acp_agents_test.go) from the real ACP adapter and scripts/fixtures/acp-fake.mjs. Do not edit.", "events": append(catalogs, events...), "pending": pending}, "", "  ")
	path := "../../../../../scripts/fixtures/acp-journal.json"
	if os.Getenv("LAZYBOX_UPDATE_ACP_JOURNAL") == "1" {
		if err = os.WriteFile(path, append(got, '\n'), 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	want, err := os.ReadFile(path)
	if err != nil || strings.TrimSpace(string(want)) != strings.TrimSpace(string(got)) {
		t.Fatalf("scripts/fixtures/acp-journal.json is not what the ACP adapter journals now; regenerate it with LAZYBOX_UPDATE_ACP_JOURNAL=1 go test ./internal/runner -run TestACPJournalFixtureIsCurrent (%v)\n%s", err, got)
	}
}
