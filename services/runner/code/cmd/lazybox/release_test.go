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

// Updating the installed service is a no-op for what runs in it: a terminal
// started under one release's Runner is still running, and re-attached, under
// the next one's. This installs a uniquely labelled launchd job for this
// user, so it also needs LAZYBOX_V2_RELEASE_LAUNCHD=1.
func TestReleaseServiceUpdateKeepsWhatRuns(t *testing.T) {
	runtime := releaseRuntime(t)
	if os.Getenv("LAZYBOX_V2_RELEASE_LAUNCHD") != "1" {
		t.Skip("set LAZYBOX_V2_RELEASE_LAUNCHD=1 to install a test launchd job")
	}
	suffix := make([]byte, 4)
	_, _ = rand.Read(suffix)
	label := "ai.lazybox.runner.test-" + hex.EncodeToString(suffix)
	home := t.TempDir()
	env := []string{"HOME=" + home, "PATH=/usr/bin:/bin", "SHELL=/bin/sh", installation.LabelEnvironment + "=" + label}
	paths, err := installation.UserPaths(func(k string) string {
		return map[string]string{"HOME": home, installation.LabelEnvironment: label}[k]
	})
	if err != nil {
		t.Fatal(err)
	}
	uid := strconv.Itoa(os.Getuid())
	t.Cleanup(func() {
		// Whatever happened, no test job stays loaded.
		_ = exec.Command("/bin/launchctl", "bootout", "gui/"+uid+"/"+label).Run()
	})

	// Two copies of the release, as an update replaces the app: clones, so the copy is instant on APFS.
	copyRuntime := func(name string) installation.Layout {
		dir := filepath.Join(t.TempDir(), name)
		if out, err := exec.Command("/bin/cp", "-cR", runtime.Root, dir).CombinedOutput(); err != nil {
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
	hostOf := func(c genconnect.RunnerServiceClient) *pb.HostedProcess {
		inventory, err := c.ListProcesses(ctx, connect.NewRequest(&pb.ListProcessesRequest{}))
		if err != nil {
			t.Fatal(err)
		}
		for _, p := range inventory.Msg.Processes {
			if p.Kind == "terminal" {
				return p
			}
		}
		return nil
	}
	before := hostOf(client)
	if before == nil || before.ProgramPid == 0 {
		t.Fatalf("no terminal host: %+v", before)
	}

	// The update: the job now runs the second copy; launchd stops the first Runner.
	out = runIn(t, env, second.CLI(), "service", "install")
	t.Log(out)
	if !strings.Contains(out, "re-attached: 1 terminal") {
		t.Errorf("the update did not report the kept terminal:\n%s", out)
	}
	installed, err := installation.ReadPlist(ctx, paths.Plist)
	// The CLI resolves its own location through symbolic links (/var is /private/var).
	wantRunner, _ := filepath.EvalSymlinks(second.Runner())
	if err != nil || installed.Program != wantRunner {
		t.Fatalf("the job does not run the new release: %+v %v", installed, err)
	}
	next := waitForEndpoint(t, paths.Endpoint(), tg.Token, 60*time.Second)
	client = genconnect.NewRunnerServiceClient(newHTTPClient(next.Token), next.URL)
	after := hostOf(client)
	if after == nil || after.ProgramPid != before.ProgramPid || !after.Attached || after.State != "running" {
		t.Fatalf("the terminal did not survive the update: before %+v, after %+v", before, after)
	}
	if syscall.Kill(int(before.ProgramPid), 0) != nil {
		t.Fatal("the terminal's shell is gone")
	}

	if _, err = client.CloseTerminal(ctx, connect.NewRequest(&pb.TerminalRequest{TerminalId: terminal.Msg.Id})); err != nil {
		t.Fatal(err)
	}
	runIn(t, env, second.CLI(), "service", "uninstall")
	if _, err = os.Stat(paths.Plist); err == nil {
		t.Fatal("uninstall left the plist")
	}
}
