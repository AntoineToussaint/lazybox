package transport

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Version says which lazybox agents' shells reach, for Settings → About and
// `lazybox doctor`: here a ~/.zshenv puts a V1 lazybox first and the shell
// integration puts this Runner's back.
func TestVersionReportsWhichLazyboxAgentsReach(t *testing.T) {
	zsh, err := exec.LookPath("zsh")
	if err != nil {
		t.Skip("no zsh")
	}
	root := t.TempDir()
	home, cli := filepath.Join(root, "home"), filepath.Join(root, "cli")
	v1 := filepath.Join(home, ".cargo", "bin")
	for _, dir := range []string{v1, cli} {
		if err = os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err = os.WriteFile(filepath.Join(dir, "lazybox"), []byte("#!/bin/sh\n"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	if err = os.WriteFile(filepath.Join(home, ".zshenv"), []byte("typeset -U path\npath=(\"$HOME/.cargo/bin\" $path)\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("HOME", home)
	t.Setenv("SHELL", zsh)
	t.Setenv("ZDOTDIR", "")
	os.Unsetenv("ZDOTDIR")
	b, err := runner.New(runner.Config{State: filepath.Join(root, "state"), CLIDir: cli})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	s := &Service{Bench: b}
	v, err := s.Version(context.Background(), &pb.VersionRequest{WaitAgentCli: true})
	if err != nil {
		t.Fatal(err)
	}
	a := v.AgentCli
	if a == nil || a.Cli != filepath.Join(cli, "lazybox") || a.Shadow != filepath.Join(v1, "lazybox") || !a.Fixed || len(a.Unfixed) != 0 || !strings.Contains(a.Notice, "nothing to do") || a.CheckedAtUnixMillis == 0 {
		t.Fatalf("agent CLI %+v", a)
	}
	// Without waiting, the last probe is returned at once.
	if v, err = s.Version(context.Background(), &pb.VersionRequest{}); err != nil || v.AgentCli == nil || v.AgentCli.Shadow != a.Shadow {
		t.Fatalf("cached: %+v %v", v, err)
	}
}
