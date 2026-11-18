package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// These run against a built release (scripts/release/build.mjs, `make -C v2
// release` runs them): LAZYBOX_V2_RELEASE_RUNTIME names the runtime directory
// inside the built Lazybox.app. Without it they are skipped.

func releaseRuntime(t *testing.T) installation.Layout {
	t.Helper()
	root := os.Getenv("LAZYBOX_V2_RELEASE_RUNTIME")
	if root == "" {
		t.Skip("set LAZYBOX_V2_RELEASE_RUNTIME to a built release's runtime directory")
	}
	l := installation.Layout{Root: root}
	if err := l.Check(); err != nil {
		t.Fatal(err)
	}
	return l
}

func runIn(t *testing.T, env []string, program string, args ...string) string {
	t.Helper()
	cmd := exec.Command(program, args...)
	cmd.Env = env
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("%s %v: %v\n%s", filepath.Base(program), args, err, out)
	}
	return string(out)
}

func waitForEndpoint(t *testing.T, path, previous string, within time.Duration) target {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), within)
	defer cancel()
	if _, err := waitForRunner(ctx, path, previous); err != nil {
		t.Fatalf("no Runner answered at %s: %v", path, err)
	}
	tg, err := loadTarget("", path, false)
	if err != nil {
		t.Fatal(err)
	}
	return tg
}

// The standalone Runner a release ships starts with nothing but a state
// directory, answers the CLI it ships with the same version, and stops cleanly.
func TestReleaseRunnerServesAndStops(t *testing.T) {
	runtime := releaseRuntime(t)
	release, _ := runtime.ReadVersion()
	home := t.TempDir()
	state := filepath.Join(home, "state")
	env := []string{"HOME=" + home, "PATH=/usr/bin:/bin"}
	cmd := exec.Command(runtime.Runner(), "serve", "--state", state, "--config", filepath.Join(home, "runner.config.json"), "--worktrees", filepath.Join(home, "wt"), "--actions", filepath.Join(home, "actions.yaml"))
	cmd.Env = env
	log, _ := os.Create(filepath.Join(home, "runner.log"))
	cmd.Stdout, cmd.Stderr = log, log
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	exited := make(chan error, 1)
	go func() { exited <- cmd.Wait() }()
	t.Cleanup(func() { _ = cmd.Process.Kill() })
	endpoint := filepath.Join(state, "endpoint.json")
	tg := waitForEndpoint(t, endpoint, "", 60*time.Second)
	if !strings.HasPrefix(tg.URL, "http://localhost:") {
		t.Fatalf("endpoint %s", tg.URL)
	}
	if info, err := os.Stat(endpoint); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("endpoint file mode: %v %v", info.Mode(), err)
	}
	if got := strings.TrimSpace(runIn(t, env, runtime.CLI(), "--version")); got != "lazybox "+release+" (Lazybox V2)" {
		t.Fatalf("CLI version %q, release %s", got, release)
	}
	v, err := dial(tg).Version(context.Background(), connect.NewRequest(&pb.VersionRequest{}))
	if err != nil || v.Msg.Version != release {
		t.Fatalf("Runner version %v %v, release %s", v, err, release)
	}
	ls := runIn(t, env, runtime.CLI(), "--endpoint", endpoint, "ls", "--json")
	if !json.Valid([]byte(ls)) {
		t.Fatalf("ls: %s", ls)
	}
	if err = cmd.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatal(err)
	}
	select {
	case err = <-exited:
		if err != nil {
			data, _ := os.ReadFile(filepath.Join(home, "runner.log"))
			t.Fatalf("the Runner did not stop cleanly: %v\n%s", err, data)
		}
	case <-time.After(40 * time.Second):
		t.Fatal("the Runner did not stop within 40s of SIGTERM")
	}
}

// Updating the installed service is a no-op for what runs in it: a terminal,
// an agent in the middle of a turn (the fake ACP agent, run by the release's
// own Node) and a managed command started under one release's Runner are
// still running, and re-attached, under the next one's, and again after a
// plain `lazybox service restart`. This installs a uniquely labelled service for this user
// with the platform's manager (a launchd job; a systemd user unit; the
// supervisor where there is no systemd, or with LAZYBOX_SERVICE_MANAGER), so
// it also needs LAZYBOX_V2_RELEASE_SERVICE=1 (LAZYBOX_V2_RELEASE_LAUNCHD=1
// is the older name).
func TestReleaseServiceUpdateKeepsWhatRuns(t *testing.T) {
	runtime := releaseRuntime(t)
	if os.Getenv("LAZYBOX_V2_RELEASE_SERVICE") != "1" && os.Getenv("LAZYBOX_V2_RELEASE_LAUNCHD") != "1" {
		t.Skip("set LAZYBOX_V2_RELEASE_SERVICE=1 to install a test service")
	}
	suffix := make([]byte, 4)
	_, _ = rand.Read(suffix)
	label := "ai.lazybox.runner.test-" + hex.EncodeToString(suffix)
	home := t.TempDir()
	settings := map[string]string{"HOME": home, installation.LabelEnvironment: label, installation.ManagerEnvironment: os.Getenv(installation.ManagerEnvironment)}
	manager, err := installation.SystemManager(func(k string) string { return settings[k] })
	if err != nil {
		t.Fatal(err)
	}
	if manager.Kind() == installation.ManagerSystemd {
		// The user manager reads units from the person's own configuration
		// directory, whatever HOME this test gives the CLI. The label is
		// unique and the unit is removed below.
		real, err := os.UserConfigDir()
		if err != nil {
			t.Fatal(err)
		}
		settings["XDG_CONFIG_HOME"] = real
	}
	env := []string{"PATH=/usr/bin:/bin", "SHELL=/bin/sh"}
	for k, v := range settings {
		if v != "" {
			env = append(env, k+"="+v)
		}
	}
	// systemctl --user and systemd-run --user find the user manager through these.
	for _, k := range []string{"XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "USER"} {
		if v := os.Getenv(k); v != "" {
			env = append(env, k+"="+v)
		}
	}
	paths, err := installation.UserPaths(func(k string) string { return settings[k] })
	if err != nil {
		t.Fatal(err)
	}
	uid := strconv.Itoa(os.Getuid())
	t.Cleanup(func() {
		// Whatever happened, no test service stays loaded.
		switch manager.Kind() {
		case installation.ManagerLaunchd:
			_ = exec.Command("/bin/launchctl", "bootout", "gui/"+uid+"/"+label).Run()
		default:
			_ = manager.Remove(context.Background(), paths)
			_ = os.Remove(manager.DefinitionPath(paths))
			_ = os.Remove(paths.Locator())
			if manager.Kind() == installation.ManagerSystemd {
				_ = exec.Command("systemctl", "--user", "daemon-reload").Run()
			}
		}
	})

	// Two copies of the release, as an update replaces the app: clones, so the copy is instant on APFS.
	copyRuntime := func(name string) installation.Layout {
		dir := filepath.Join(t.TempDir(), name)
		args := []string{"-cR", runtime.Root, dir}
		if manager.Kind() != installation.ManagerLaunchd {
			args = []string{"-a", runtime.Root, dir}
		}
		if out, err := exec.Command("/bin/cp", args...).CombinedOutput(); err != nil {
			t.Fatalf("copy runtime: %v %s", err, out)
		}
		return installation.Layout{Root: dir}
	}
	first, second := copyRuntime("first"), copyRuntime("second")

	// A repository the workspace's terminal runs in.
	repo := filepath.Join(home, "repo")
	for _, args := range [][]string{{"init", "-b", "main", repo}, {"-C", repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", args...).CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
	}
	if err = os.MkdirAll(paths.Support, 0o700); err != nil {
		t.Fatal(err)
	}
	config, _ := json.Marshal(map[string]any{"profiles": []any{}, "repositories": []map[string]string{{"id": "fixture", "path": repo}}})
	if err = os.WriteFile(paths.Config, config, 0o600); err != nil {
		t.Fatal(err)
	}

	out := runIn(t, env, first.CLI(), "service", "install")
	t.Log(out)
	tg := waitForEndpoint(t, paths.Endpoint(), "", 60*time.Second)
	client := genconnect.NewRunnerServiceClient(newHTTPClient(tg.Token), tg.URL)
	ctx := context.Background()
	created, err := client.CreateCollection(ctx, connect.NewRequest(&pb.CreateCollectionRequest{RequestKey: "smoke", Spec: &pb.CollectionSpec{Name: "Smoke", Members: []*pb.WorktreeSpec{{Name: "fixture", RepositoryId: "fixture"}}}}))
	if err != nil {
		t.Fatal(err)
	}
	terminal, err := client.OpenTerminal(ctx, connect.NewRequest(&pb.OpenTerminalRequest{CollectionId: created.Msg.Collection.Id, Cols: 80, Rows: 24, RequestKey: "smoke"}))
	if err != nil {
		t.Fatal(err)
	}
	processOf := func(c genconnect.RunnerServiceClient, kind string) *pb.HostedProcess {
		inventory, err := c.ListProcesses(ctx, connect.NewRequest(&pb.ListProcessesRequest{}))
		if err != nil {
			t.Fatal(err)
		}
		for _, p := range inventory.Msg.Processes {
			if p.Kind == kind {
				return p
			}
		}
		return nil
	}
	hostOf := func(c genconnect.RunnerServiceClient) *pb.HostedProcess { return processOf(c, "terminal") }
	before := hostOf(client)
	if before == nil || before.ProgramPid == 0 {
		t.Fatalf("no terminal host: %+v", before)
	}
	// An agent mid-turn: the fake ACP agent waits until the turn is cancelled.
	script, err := filepath.Abs("../../../../../scripts/fixtures/acp-fake.mjs")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = client.SaveAcpAgent(ctx, connect.NewRequest(&pb.SaveAcpAgentRequest{Id: "fake", Title: "Fake agent", Command: first.Node(), Args: []string{script}})); err != nil {
		t.Fatal(err)
	}
	opened, err := client.OpenSession(ctx, connect.NewRequest(&pb.OpenSessionRequest{ProfileId: "acp-fake", EmptyWorkspace: true, Options: &pb.RunnerOptions{Permission: "ask"}}))
	if err != nil {
		t.Fatal(err)
	}
	session := opened.Msg.Session.Id
	if _, err = client.Apply(ctx, connect.NewRequest(&pb.ApplyRequest{SessionId: session, OperationId: "wait", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "wait"}}})); err != nil {
		t.Fatal(err)
	}
	command, err := client.StartCommand(ctx, connect.NewRequest(&pb.StartCommandRequest{SessionId: session, Title: "sleep", Argv: []string{"sleep", "600"}}))
	if err != nil {
		t.Fatal(err)
	}
	var agentBefore, commandBefore *pb.HostedProcess
	for end := time.Now().Add(30 * time.Second); time.Now().Before(end) && (agentBefore == nil || agentBefore.ProgramPid == 0 || commandBefore == nil || commandBefore.ProgramPid == 0); time.Sleep(100 * time.Millisecond) {
		agentBefore, commandBefore = processOf(client, "agent"), processOf(client, "command")
	}
	if agentBefore == nil || agentBefore.ProgramPid == 0 || commandBefore == nil || commandBefore.ProgramPid == 0 {
		t.Fatalf("no agent or command host: %+v %+v", agentBefore, commandBefore)
	}
	busy := func(c genconnect.RunnerServiceClient) bool {
		got, err := c.GetSession(ctx, connect.NewRequest(&pb.SessionRequest{SessionId: session}))
		return err == nil && got.Msg.Session.Busy && !got.Msg.Session.Closed
	}
	if !busy(client) {
		t.Fatal("the agent's turn is not running")
	}
	// survived checks that what ran before a Runner restart runs on, with
	// the same programs, held by the new Runner, the turn still going.
	survived := func(c genconnect.RunnerServiceClient, when string) {
		t.Helper()
		for _, want := range []*pb.HostedProcess{before, agentBefore, commandBefore} {
			var got *pb.HostedProcess
			for end := time.Now().Add(20 * time.Second); time.Now().Before(end); time.Sleep(100 * time.Millisecond) {
				if got = processOf(c, want.Kind); got != nil && got.Attached {
					break
				}
			}
			if got == nil || got.ProgramPid != want.ProgramPid || !got.Attached || got.State != "running" {
				t.Fatalf("the %s did not survive %s: before %+v, after %+v", want.Kind, when, want, got)
			}
			if syscall.Kill(int(want.ProgramPid), 0) != nil {
				t.Fatalf("the %s's program is gone after %s", want.Kind, when)
			}
		}
		if !busy(c) {
			t.Fatalf("the agent's turn ended with %s", when)
		}
	}

	// The update: the service now runs the second copy; its manager stops the first Runner.
	out = runIn(t, env, second.CLI(), "service", "install")
	t.Log(out)
	if !strings.Contains(out, "re-attached: 1 agent, 1 command and 1 terminal") {
		t.Errorf("the update did not report what it kept:\n%s", out)
	}
	installed, err := manager.ReadDefinition(ctx, manager.DefinitionPath(paths))
	// The CLI resolves its own location through symbolic links (/var is /private/var).
	wantRunner, _ := filepath.EvalSymlinks(second.Runner())
	if err != nil || installed.Program != wantRunner {
		t.Fatalf("the job does not run the new release: %+v %v", installed, err)
	}
	next := waitForEndpoint(t, paths.Endpoint(), tg.Token, 60*time.Second)
	client = genconnect.NewRunnerServiceClient(newHTTPClient(next.Token), next.URL)
	survived(client, "the update")
	if manager.Kind() == installation.ManagerSystemd {
		// Under systemd each runs in a scope of its own, outside the unit's cgroup.
		cgroup, _ := os.ReadFile("/proc/" + strconv.Itoa(int(agentBefore.HostPid)) + "/cgroup")
		if !strings.Contains(string(cgroup), "/lazybox-host-") {
			t.Errorf("the agent's host is not in a scope of its own: %s", cgroup)
		}
	}

	// A plain restart of the same release keeps them too.
	runIn(t, env, second.CLI(), "service", "restart")
	again := waitForEndpoint(t, paths.Endpoint(), next.Token, 60*time.Second)
	client = genconnect.NewRunnerServiceClient(newHTTPClient(again.Token), again.URL)
	survived(client, "a restart")

	if _, err = client.CancelCommand(ctx, connect.NewRequest(&pb.CommandRequest{CommandId: command.Msg.Id})); err != nil {
		t.Fatal(err)
	}
	if _, err = client.CloseSession(ctx, connect.NewRequest(&pb.SessionRequest{SessionId: session})); err != nil {
		t.Fatal(err)
	}
	if _, err = client.CloseTerminal(ctx, connect.NewRequest(&pb.TerminalRequest{TerminalId: terminal.Msg.Id})); err != nil {
		t.Fatal(err)
	}
	runIn(t, env, second.CLI(), "service", "uninstall")
	if _, err = os.Stat(manager.DefinitionPath(paths)); err == nil {
		t.Fatal("uninstall left the service definition")
	}
}
