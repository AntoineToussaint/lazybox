package transport

import (
	"context"
	"flag"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"connectrpc.com/vanguard/vanguardgrpc"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/testselect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
)

// cliBuild is this build's lazybox CLI. Compiling is integration work, so
// TestMain does it once for the package, before any test runs, and no test
// pays for it; its time is bounded by go test's -timeout for the package, not
// by the per-test deadline. It is built only when its test will run: zsh is
// installed (the test skips otherwise) and -list, -run and -skip select it. It
// is built with the real HOME, so it uses the existing module cache.
var cliBuild struct {
	built bool
	dir   string
	err   error
}

const pipeInTerminalTest = "TestAPipeInAWorkspaceTerminalReachesTheWorkspaceConsole"

func zshInstalled() bool {
	_, err := os.Stat("/bin/zsh")
	return err == nil
}

func TestMain(m *testing.M) {
	flag.Parse()
	var dir string
	if zshInstalled() && testselect.WillRun(pipeInTerminalTest) {
		cliBuild.built = true
		var err error
		if dir, err = os.MkdirTemp("", "lbx-cli-"); err == nil {
			build := exec.Command("go", "build", "-o", filepath.Join(dir, "lazybox"), "github.com/AntoineToussaint/lazybox/services/runner/code/cmd/lazybox")
			if out, buildErr := build.CombinedOutput(); buildErr != nil {
				err = fmt.Errorf("go build: %v\n%s", buildErr, out)
			}
		}
		cliBuild.dir, cliBuild.err = dir, err
	}
	code := m.Run()
	if dir != "" {
		_ = os.RemoveAll(dir)
	}
	os.Exit(code)
}

// The whole path a person takes: a real zsh login shell in a workspace
// terminal, whose profile puts another `lazybox` first on PATH (as a V1
// install does), pipes a test run into `lazybox log` with no flag. The
// Runner's CLI runs, reaches the Runner over Connect with the terminal's own
// credential, and the workspace gets the run with its compact view.
func TestAPipeInAWorkspaceTerminalReachesTheWorkspaceConsole(t *testing.T) {
	if !zshInstalled() {
		t.Skip("zsh is not installed")
	}
	if !cliBuild.built {
		t.Fatal("TestMain built no CLI: its selection names " + pipeInTerminalTest)
	}
	if cliBuild.err != nil {
		t.Fatalf("TestMain could not build the CLI: %v", cliBuild.err)
	}
	cli := cliBuild.dir
	t.Setenv("SHELL", "/bin/zsh")
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("ZDOTDIR", "")
	os.Unsetenv("ZDOTDIR")
	other := filepath.Join(home, "v1")
	if err := os.MkdirAll(other, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(other, "lazybox"), []byte("#!/bin/sh\necho 'the other lazybox ran' >&2\ncat\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, ".zshrc"), []byte("path=($HOME/v1 $path)\nexport FROM_ZSHRC=yes\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	repo := t.TempDir()
	for _, args := range [][]string{{"init", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput(); err != nil {
			t.Fatalf("git fixture: %v %s", err, out)
		}
	}
	var handler http.Handler = http.NotFoundHandler()
	host := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { handler.ServeHTTP(w, r) }))
	t.Cleanup(host.Close)
	b, err := runner.New(runner.Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), WorktreeRoot: t.TempDir(), Repositories: []collections.Repository{{ID: "api", Path: repo}}, Address: host.URL, CLIDir: cli})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	server := grpc.NewServer(LocalAuthority(ownerToken, b)...)
	s := &Service{Bench: b}
	pb.RegisterRunnerServiceServer(server, s)
	transcoder, err := vanguardgrpc.NewTranscoder(server)
	if err != nil {
		t.Fatal(err)
	}
	handler = transcoder

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	created, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: "e2e", Spec: &pb.CollectionSpec{Name: "Parser", Members: []*pb.WorktreeSpec{{Name: "api", RepositoryId: "api"}}}})
	if err != nil {
		t.Fatal(err)
	}
	opened, err := s.OpenTerminal(ctx, &pb.OpenTerminalRequest{CollectionId: created.Collection.Id, Cols: 120, Rows: 40})
	if err != nil {
		t.Fatal(err)
	}
	shell, err := b.Terminals.Get(opened.Id)
	if err != nil {
		t.Fatal(err)
	}
	// The window types only once the shell says its prompt reads input, and
	// reuses a shell the list calls idle: both come over the API.
	if !opened.PromptMarks {
		t.Fatal("zsh with the shell integration says when its prompt is ready")
	}
	for {
		listed, err := s.ListTerminals(ctx, &pb.ListTerminalsRequest{CollectionId: created.Collection.Id})
		if err != nil {
			t.Fatal(err)
		}
		if w := listed.Terminals[0]; w.PromptsReady > 0 {
			if !w.Idle {
				t.Fatalf("at its first prompt, in its folder, the shell is idle: %+v", w)
			}
			break
		}
		if ctx.Err() != nil {
			t.Fatal("the shell never said its prompt was ready")
		}
		time.Sleep(50 * time.Millisecond)
	}
	script := `print -r -- "[which:$(command -v lazybox)|zshrc:$FROM_ZSHRC]"; printf 'ok   example.com/lexer 0.1s\n--- FAIL: TestParse (0.00s)\n    parser_test.go:42: want 3, got 4\n' | lazybox log --title 'go test ./...'` + "\n"
	if err = shell.Write([]byte(script)); err != nil {
		t.Fatal(err)
	}
	finished := func(s string) bool {
		return strings.Contains(s, "|zshrc:") && (strings.Contains(s, "is in the workspace console") || strings.Contains(s, "other lazybox ran") || strings.Contains(s, "not shown"))
	}
	var output strings.Builder
	for after := uint64(0); !finished(output.String()); {
		data, offset, _, err := shell.Next(ctx, after)
		if err != nil {
			t.Fatalf("%v; terminal output %q", err, output.String())
		}
		output.Write(data)
		after = offset + uint64(len(data))
	}
	want := []string{"[which:" + filepath.Join(cli, "lazybox") + "|zshrc:yes]", `"go test ./..." is in the workspace console`}
	if !containsAll(output.String(), want) || strings.Contains(output.String(), "the other lazybox ran") {
		t.Fatalf("the Runner's CLI must run, after the person's own .zshrc: %q", output.String())
	}
	runs := b.Commands.ListWorkspace(created.Collection.Id)
	if len(runs) != 1 || !runs[0].Done || runs[0].Spec.TerminalID != opened.Id || runs[0].Report == nil {
		t.Fatalf("the workspace has no finished run from this terminal: %+v", runs)
	}
	if r := runs[0].Report; !r.Piped || !strings.Contains(r.AgentView, "want 3, got 4") || r.OutputPath == "" {
		t.Fatalf("the run lacks its compact view: %+v", r)
	}
}

func containsAll(s string, parts []string) bool {
	for _, part := range parts {
		if !strings.Contains(s, part) {
			return false
		}
	}
	return true
}
