package runner

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
)

// The claude.ai connectors setting is read when a session starts: off until
// the person turns it on, and then on for the sessions started afterwards.
func TestClaudeAIConnectorsFollowTheSettingAtSessionStart(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	if fake.Session.Open.ClaudeAIConnectors {
		t.Fatal("claude.ai connectors load by default")
	}
	prefs := b.Preferences.Snapshot()
	prefs.ClaudeAIConnectors = true
	if err = b.Preferences.Save(prefs); err != nil {
		t.Fatal(err)
	}
	if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	if !fake.Session.Open.ClaudeAIConnectors {
		t.Fatal("a session started after the person turned connectors on did not get them")
	}
}

func TestAgentsGetTheCLIFirstOnPathAndTheCommandBriefing(t *testing.T) {
	for _, withCLI := range []bool{true, false} {
		cfg := Config{State: t.TempDir()}
		if withCLI {
			cfg.CLIDir = "/opt/lazybox/bin"
		}
		b, err := New(cfg)
		if err != nil {
			t.Fatal(err)
		}
		fake := &agenttest.Adapter{}
		b.Adapters["codex"] = fake
		if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err != nil {
			t.Fatal(err)
		}
		open := fake.Session.Open
		path := ""
		paths := 0
		for _, entry := range open.Env {
			if value, ok := strings.CutPrefix(entry, "PATH="); ok {
				path, paths = value, paths+1
			}
		}
		if want := "LAZYBOX_V2_OUTPUT_DIR=" + filepath.Join(cfg.State, "command-output", fake.Session.Open.ID); !contains(open.Env, want) {
			t.Fatalf("agent not given its session's output directory %q: %v", want, open.Env)
		}
		briefed := strings.Contains(open.Context, "lazybox run -- <command>")
		if briefed != strings.Contains(open.Context, "lazybox artifact put <file|-> --title") {
			t.Fatal("the briefing does not tell agents how to publish artifacts")
		}
		// Design docs and plans stay in Lazybox, one artifact per document, not in a connector's document tool.
		if briefed != (strings.Contains(open.Context, "--id ID`, never as a second artifact") && strings.Contains(open.Context, "Never put these documents in an external document tool or connector (Claude Docs")) {
			t.Fatal("the briefing does not keep design docs in Lazybox artifacts, one artifact per document")
		}
		// Overlapping artifacts are merged and the old ones retired, never left as pointer stubs.
		if briefed != (strings.Contains(open.Context, "Keep one artifact per topic") && strings.Contains(open.Context, "--supersedes ID,ID`, which retires the old ones; don't leave pointer stubs")) {
			t.Fatal("the briefing does not tell agents to merge overlapping artifacts with --supersedes")
		}
		if briefed != (strings.Contains(open.Context, "lazybox workspace repos") && strings.Contains(open.Context, `lazybox workspace request-repo <name|owner/repo|path> --reason "why you need it"`)) {
			t.Fatal("the briefing does not tell agents how to ask for a repository instead of reaching outside the workspace")
		}
		if withCLI && (paths != 1 || !strings.HasPrefix(path, "/opt/lazybox/bin"+string(os.PathListSeparator)) || !briefed) {
			t.Fatalf("CLI not offered: paths=%d path=%q briefed=%v", paths, path, briefed)
		}
		if !withCLI && (strings.Contains(path, "/opt/lazybox/bin") || briefed) {
			t.Fatal("briefing an agent about a CLI it cannot run")
		}
		b.Close()
	}
}

func contains(env []string, entry string) bool {
	for _, value := range env {
		if value == entry {
			return true
		}
	}
	return false
}

func TestRunReportIsKeptForTheConsoleAndTelemetry(t *testing.T) {
	cfg := Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir()}
	b, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	pipe, err := b.RegisterPipe(e, "go test ./...", false, nil)
	if err != nil {
		t.Fatal(err)
	}
	report := commands.Report{ExitCode: 1, OutputBytes: 1 << 20, AgentBytes: 2048, Filter: "go-test", OutputPath: filepath.Join(b.Config.OutputRoot, e.ID, "go-1.log")}
	if err = b.FinishPipe(pipe, nil, "", &report); err != nil {
		t.Fatal(err)
	}
	if got := pipe.Status().Report; got == nil || *got != report {
		t.Fatalf("console cannot label the run: %+v", got)
	}
	runs, err := b.Telemetry.(*telemetry.SQLite).Commands(context.Background(), e.ID)
	if err != nil || len(runs) != 1 || runs[0].ID != pipe.Status().ID || runs[0].AgentBytes != 2048 || runs[0].OutputBytes != 1<<20 || runs[0].Filter != "go-test" || runs[0].Scope.Conversation != e.Saved.ConversationID {
		t.Fatalf("run not measured: %v %+v", err, runs)
	}
	plain, err := b.RegisterPipe(e, "tail -f log", false, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err = b.FinishPipe(plain, nil, "", nil); err != nil || plain.Status().Report != nil {
		t.Fatalf("a plain pipe has no report: %v %+v", err, plain.Status())
	}
	if runs, _ = b.Telemetry.(*telemetry.SQLite).Commands(context.Background(), e.ID); len(runs) != 1 {
		t.Fatalf("a plain pipe is not a run: %+v", runs)
	}
}

func TestDeletingAConversationDeletesItsFullOutput(t *testing.T) {
	cfg := Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir()}
	b, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	other, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	var mine, kept []string
	for _, root := range []string{b.Config.OutputRoot, b.Config.OutputFallbackRoot} {
		for id, list := range map[string]*[]string{e.ID: &mine, other.ID: &kept} {
			path := filepath.Join(root, id, "go-1.log")
			if err = os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
				t.Fatal(err)
			}
			if err = os.WriteFile(path, []byte("unredacted"), 0o600); err != nil {
				t.Fatal(err)
			}
			*list = append(*list, path)
		}
	}
	if err = b.DeleteConversation(context.Background(), e.ID, true); err != nil {
		t.Fatal(err)
	}
	for _, path := range mine {
		if _, err = os.Stat(filepath.Dir(path)); !errors.Is(err, os.ErrNotExist) {
			t.Fatal("full output outlived its conversation", path, err)
		}
	}
	for _, path := range kept {
		if _, err = os.Stat(path); err != nil {
			t.Fatal("another conversation's output was deleted", path, err)
		}
	}
}

func TestOutputIsTrimmedOldestFirstToTheBudget(t *testing.T) {
	state, temp := t.TempDir(), t.TempDir()
	now := time.Now()
	write := func(path string, size int, age time.Duration) string {
		if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, make([]byte, size), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := os.Chtimes(path, now.Add(-age), now.Add(-age)); err != nil {
			t.Fatal(err)
		}
		return path
	}
	oldest := write(filepath.Join(state, "a", "1.log"), 100, 5*time.Hour)
	older := write(filepath.Join(temp, "b", "2.log"), 100, 4*time.Hour)
	legacy := write(filepath.Join(temp, "3.log"), 100, 3*time.Hour)
	recent := write(filepath.Join(state, "a", "4.log"), 100, 2*time.Hour)
	fresh := write(filepath.Join(state, "c", "5.log"), 500, time.Minute)
	if err := trimOutput([]string{state, temp, filepath.Join(state, "absent")}, 650, now.Add(-time.Hour)); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{oldest, older, legacy} {
		if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
			t.Fatal("over budget, the oldest file should go", path, err)
		}
	}
	if _, err := os.Stat(filepath.Dir(older)); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("an emptied session directory should go", err)
	}
	for _, path := range []string{recent, fresh, temp, state} {
		if _, err := os.Stat(path); err != nil {
			t.Fatal("trimmed more than the budget needs", path, err)
		}
	}
	// A file the agent was just pointed at stays, even alone over budget.
	if err := trimOutput([]string{state, temp}, 10, now.Add(-time.Hour)); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(recent); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("an old file survived over budget", err)
	}
	if _, err := os.Stat(fresh); err != nil {
		t.Fatal("a fresh file was trimmed", err)
	}
}
