package integration

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

func restartRepositories(t *testing.T) ([]collections.Repository, []collections.MemberSpec) {
	t.Helper()
	var repos []collections.Repository
	var members []collections.MemberSpec
	for _, name := range []string{"api", "web"} {
		repo := t.TempDir()
		if err := os.WriteFile(filepath.Join(repo, "tracked"), []byte("initial"), 0600); err != nil {
			t.Fatal(err)
		}
		for _, args := range [][]string{{"init", "-b", "main"}, {"add", "tracked"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "initial"}} {
			if out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput(); err != nil {
				t.Fatalf("fixture git: %s %v", out, err)
			}
		}
		repos = append(repos, collections.Repository{ID: name, Path: repo})
		members = append(members, collections.MemberSpec{Name: name, RepositoryID: name, BaseRef: "refs/heads/main"})
	}
	return repos, members
}
func writeRestartFiles(t *testing.T, workspace collections.Collection, stage string) {
	t.Helper()
	for _, m := range workspace.Members {
		for _, file := range []string{"tracked", "untracked"} {
			if err := os.WriteFile(filepath.Join(m.Path, file), []byte(m.Name+file+stage), 0600); err != nil {
				t.Fatal(err)
			}
		}
	}
}
func verifyRestartFiles(t *testing.T, workspace collections.Collection, stage string) {
	t.Helper()
	if len(workspace.Members) != 2 {
		t.Fatalf("expected two repositories, got %d", len(workspace.Members))
	}
	for _, m := range workspace.Members {
		for _, file := range []string{"tracked", "untracked"} {
			got, err := os.ReadFile(filepath.Join(m.Path, file))
			if err != nil || string(got) != m.Name+file+stage {
				t.Fatalf("%s/%s: %q, %v", m.Name, file, got, err)
			}
		}
	}
}

func TestInstalledCodexRestart(t *testing.T) {
	if _, err := exec.LookPath("codex"); err != nil {
		t.Skip("Codex CLI required")
	}
	root := t.TempDir()
	t.Setenv("RESTART_FIXTURE_TOKEN", "local-fixture")
	var inspected atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(io.LimitReader(r.Body, 4<<20))
		reply := "fixture reply"
		if strings.Contains(string(body), `"text":"inspect-child"`) {
			inspected.Add(1)
			if !strings.Contains(string(body), `"text":"first-marker"`) || strings.Contains(string(body), `"text":"later-marker"`) {
				t.Error("child received incorrect native history")
			}
			reply = "child verified"
		}
		item := map[string]any{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": []any{map[string]string{"type": "output_text", "text": reply}}}
		w.Header().Set("Content-Type", "text/event-stream")
		for _, event := range []map[string]any{{"type": "response.created", "response": map[string]string{"id": "resp_1", "status": "in_progress"}}, {"type": "response.output_item.done", "item": item}, {"type": "response.completed", "response": map[string]any{"id": "resp_1", "status": "completed", "output": []any{item}}}} {
			raw, _ := json.Marshal(event)
			fmt.Fprintf(w, "data: %s\n\n", raw)
		}
	}))
	defer server.Close()
	repos, members := restartRepositories(t)
	home := filepath.Join(root, "profile")
	if err := os.MkdirAll(home, 0700); err != nil {
		t.Fatal(err)
	}
	b, err := runner.New(runner.Config{State: root, WorktreeRoot: t.TempDir(), Repositories: repos, Profiles: []accounts.Profile{{ID: "fixture", Provider: "codex", Isolation: "environment", Home: home, EnvRefs: map[string]string{"RESTART_FIXTURE_TOKEN": "RESTART_FIXTURE_TOKEN"}}}, Gateways: []runner.Gateway{{Provider: "codex", Gateway: agent.Gateway{ID: "fixture", URL: server.URL, TokenEnv: "RESTART_FIXTURE_TOKEN"}}}})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 35*time.Second)
	defer cancel()
	ws, err := b.Collections.Create(ctx, "source", collections.Spec{Name: "Source", Members: members})
	if err != nil {
		t.Fatal(err)
	}
	source, err := b.OpenCollection("fixture", ws.ID, "", accounts.Options{Model: "codex-fixture"}, "fixture", "")
	if err != nil {
		t.Fatal(err)
	}
	send := func(e *runner.Execution, text string, cursor *uint64) uint64 {
		t.Helper()
		if _, err := e.Engine.Apply(ctx, agent.Operation{ID: text, Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
		var prompt uint64
		for {
			record, err := e.Engine.Events(ctx, *cursor)
			if err != nil {
				t.Fatal(err)
			}
			*cursor = record.Sequence
			var ev agent.Event
			if err := json.Unmarshal(record.Value, &ev); err != nil {
				t.Fatal(err)
			}
			if ev.Type == "operation_accepted" && ev.Text == text {
				prompt = *cursor
			}
			if ev.Type == "failure" {
				t.Fatal(ev.Text)
			}
			if ev.Type == "turn_ended" && prompt != 0 {
				if ev.Status != "completed" {
					t.Fatal(ev.Status)
				}
				return prompt
			}
		}
	}
	var cursor uint64
	send(source, "first-marker", &cursor)
	writeRestartFiles(t, ws, "before")
	before := send(source, "later-marker", &cursor)
	writeRestartFiles(t, ws, "after")
	if err := b.CheckRestartBoundary(ctx, source.ID, before); err != nil {
		t.Fatal(err)
	}
	for _, isolated := range []bool{true, false} {
		child, err := b.ForkConversation(ctx, source.ID, "", before, isolated, fmt.Sprint("request-", isolated))
		if err != nil {
			t.Fatal(err)
		}
		childWS, err := b.Collections.Get(child.CollectionID)
		if err != nil {
			t.Fatal(err)
		}
		if isolated {
			verifyRestartFiles(t, childWS, "before")
			writeRestartFiles(t, childWS, "child")
			verifyRestartFiles(t, ws, "after")
		} else {
			verifyRestartFiles(t, childWS, "after")
		}
		var childCursor uint64
		revised := send(child, "inspect-child", &childCursor)
		if isolated {
			grandchild, err := b.ForkConversation(ctx, child.ID, "", revised, true, "second-revision")
			if err != nil {
				t.Fatal(err)
			}
			var nextCursor uint64
			send(grandchild, "inspect-child", &nextCursor)
		}
	}
	if inspected.Load() != 3 {
		t.Errorf("checked %d child turns, want 3", inspected.Load())
	}
}
