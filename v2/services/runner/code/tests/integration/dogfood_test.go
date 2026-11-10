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
	"github.com/AntoineToussaint/lazybox/v2/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/sessionstore"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
)

// Explicit live gate: reuse one selected managed account without enrolling,
// copying credentials, changing permissions or writing into an existing project.
func TestRealDogfoodContinuity(t *testing.T) {
	profileID := os.Getenv("LAZYBOX_V2_DOGFOOD_PROFILE")
	subscriptionState := os.Getenv("LAZYBOX_V2_DOGFOOD_SUBSCRIPTIONS")
	if os.Getenv("LAZYBOX_V2_REAL") != "1" || profileID == "" || subscriptionState == "" {
		t.Skip("explicit managed profile and subscription state required")
	}
	profiles, err := accounts.LoadProfiles(subscriptionState)
	if err != nil {
		t.Fatal(err)
	}
	var profile accounts.Profile
	for _, p := range profiles {
		if p.ID == profileID {
			profile = p
		}
	}
	if !profile.Managed || profile.Isolation != "environment" {
		t.Fatal("requires an existing isolated managed profile")
	}
	version, err := exec.Command(profile.Provider, "--version").Output()
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("provider: %s", strings.TrimSpace(string(version)))
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	root := t.TempDir()
	repos := []collections.Repository{}
	for _, name := range []string{"api", "web"} {
		path := filepath.Join(root, name)
		if err = os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
		if err = os.WriteFile(filepath.Join(path, "contract.txt"), []byte(map[string]string{"api": "ORCHID", "web": "LANTERN"}[name]+"\n"), 0600); err != nil {
			t.Fatal(err)
		}
		for _, args := range [][]string{{"init"}, {"add", "contract.txt"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "fixture"}} {
			cmd := exec.Command("git", append([]string{"-C", path}, args...)...)
			if out, err := cmd.CombinedOutput(); err != nil {
				t.Fatalf("fixture git: %v %s", err, out)
			}
		}
		repos = append(repos, collections.Repository{ID: name, Path: path})
	}
	cfg := runner.Config{State: filepath.Join(root, "state"), SubscriptionState: subscriptionState, WorktreeRoot: filepath.Join(root, "workspaces"), Repositories: repos, Worker: os.Getenv("LAZYBOX_V2_WORKER")}
	b, err := runner.New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if b != nil {
			_ = b.Close()
		}
	}()
	workspace, err := b.Collections.Create(ctx, "dogfood", collections.Spec{Name: "Dogfood continuity", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}, {Name: "web", RepositoryID: "web"}}})
	if err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenCollection(profileID, workspace.ID, "", accounts.Options{}, "metered", "")
	if err != nil {
		t.Fatal(err)
	}
	turn := func(e *runner.Execution, text string) string {
		t.Helper()
		if _, err := e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
		cursor := uint64(0)
		var response strings.Builder
		for {
			record, err := e.Engine.Events(ctx, cursor)
			if err != nil {
				t.Fatal(err)
			}
			cursor = record.Sequence
			var event agent.Event
			if err = json.Unmarshal(record.Value, &event); err != nil {
				t.Fatal(err)
			}
			switch event.Type {
			case "message_delta":
				response.WriteString(event.Text)
			case "approval", "question":
				t.Fatal("unexpected approval/input request; no permission was granted")
			case "failure":
				t.Fatalf("provider failure: %s %s", event.Status, event.Text)
			case "turn_ended":
				if event.Status != "completed" {
					t.Fatal("incomplete provider turn", event.Status)
				}
				return response.String()
			}
		}
	}
	first := turn(e, "Read api/contract.txt and web/contract.txt. Do not edit files, run shell commands, or delegate. Remember both words, and reply only with the api word followed by a space and the web word.")
	if strings.TrimSpace(first) != "ORCHID LANTERN" {
		t.Fatalf("unexpected first response: %q", first)
	}
	if err = b.History.Rename(ctx, e.ID, "Dogfood retained conversation"); err != nil {
		t.Fatal(err)
	}
	identity, err := b.History.Identity(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	b, err = runner.New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	after, err := b.History.Identity(ctx)
	if err != nil || identity != after {
		t.Fatal("Runner identity lost", err)
	}
	next, err := b.Resume(ctx, e.ID, false)
	if err != nil {
		t.Fatal(err)
	}
	if got := turn(next, "Without tools or delegation, reply only with the api word followed by a space and the web word you read earlier."); strings.TrimSpace(got) != "ORCHID LANTERN" {
		t.Fatalf("native conversation memory lost: %q", got)
	}
	records, err := b.History.Query(ctx, sessionstore.Query{Limit: 10, Workspace: &workspace.ID, Search: "retained"})
	if err != nil || len(records) != 1 || records[0].ID != next.ID {
		t.Fatal("conversation chain/title lost", err)
	}
	report, err := b.Telemetry.Report(ctx, telemetry.Query{Workspace: workspace.ID})
	if err != nil {
		t.Fatal(err)
	}
	if len(report.Recent) < 2 {
		t.Fatal("missing metered records")
	}
	for _, member := range workspace.Members {
		status, err := (gitinspect.Git{}).Status(ctx, member.Path)
		if err != nil || len(status.Files) != 0 {
			t.Fatal("provider modified fixture", err)
		}
	}
	t.Logf("PASS: two repositories read, real native resume after Runner restart, one named conversation, %d retained metered records", len(report.Recent))
}
