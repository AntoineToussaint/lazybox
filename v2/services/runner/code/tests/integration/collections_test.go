package integration

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
)

func TestRealClaudeReadsAcrossCollectionRepositories(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_REAL") != "1" {
		t.Skip("requires explicit live-provider opt-in")
	}
	if selected := os.Getenv("LAZYBOX_V2_REAL_PROVIDER"); selected != "" && selected != "claude" {
		t.Skip("different provider selected")
	}
	var repos []collections.Repository
	var members []collections.MemberSpec
	var values []string
	for _, name := range []string{"api", "web"} {
		path := t.TempDir()
		value := commands.ID()
		values = append(values, value)
		if err := os.WriteFile(filepath.Join(path, "identity.txt"), []byte(value), 0600); err != nil {
			t.Fatal(err)
		}
		for _, args := range [][]string{{"init", "-b", "main"}, {"add", "identity.txt"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "initial"}} {
			cmd := exec.Command("git", append([]string{"-C", path}, args...)...)
			if data, err := cmd.CombinedOutput(); err != nil {
				t.Fatalf("fixture git: %v %s", err, data)
			}
		}
		repos = append(repos, collections.Repository{ID: name, Path: path})
		members = append(members, collections.MemberSpec{Name: name, RepositoryID: name})
	}
	b, err := runner.New(runner.Config{State: t.TempDir(), WorktreeRoot: t.TempDir(), Repositories: repos, Worker: os.Getenv("LAZYBOX_V2_WORKER")})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	c, err := b.Collections.Create(ctx, "live-collection", collections.Spec{Name: "Isolated live fixture", Members: members})
	if err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenCollection("claude-default", c.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	_, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: "Read api/identity.txt and web/identity.txt from the Collection. Reply only with their exact contents separated by a plus sign, api first. Do not modify files or run commands."})
	if err != nil {
		t.Fatal(err)
	}
	cursor := uint64(0)
	var text strings.Builder
	for {
		r, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = r.Sequence
		var event agent.Event
		if err := json.Unmarshal(r.Value, &event); err != nil {
			t.Fatal(err)
		}
		if event.Type == "failure" {
			t.Fatalf("provider failed: %s %s", event.Status, event.Text)
		}
		if event.Type == "approval" || event.Type == "question" {
			t.Fatal("unexpected interactive request in read-only fixture", event.Text)
		}
		if event.Type == "message_delta" {
			text.WriteString(event.Text)
		}
		if event.Type == "turn_ended" {
			break
		}
	}
	if strings.TrimSpace(text.String()) != strings.Join(values, "+") {
		t.Fatalf("did not read both repository worktrees: %q", text.String())
	}
	t.Log("actual Claude SDK read two separate repository worktrees through one Collection execution; isolated temporary state, no desktop session used")
}
