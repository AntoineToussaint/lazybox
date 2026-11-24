package runner

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
)

// Each verification checkout is a full repository plus whatever its test
// built. Once its command finishes it is removed; a running one is kept.
func TestFinishedVerificationCheckoutsAreRemoved(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	source := t.TempDir()
	for _, args := range [][]string{{"init", "-q", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-q", "--allow-empty", "-m", "initial"}} {
		cmd := exec.Command("git", append([]string{"-C", source}, args...)...)
		cmd.Env = append(os.Environ(), "GIT_CONFIG_GLOBAL=/dev/null", "GIT_CONFIG_NOSYSTEM=1")
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v: %s", args, err, out)
		}
	}
	checkpoints, err := codecheckpoints.Open(filepath.Join(t.TempDir(), "checkpoints"), codecheckpoints.Limits{})
	if err != nil {
		t.Fatal(err)
	}
	manager, err := commands.New(t.TempDir(), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = manager.Close() })
	b := &Bench{Commands: manager, Checkpoints: checkpoints}

	finished, err := checkpoints.VerificationCheckout(ctx, "conv", source, "HEAD")
	if err != nil {
		t.Fatal(err)
	}
	done, err := manager.Start(commands.Spec{SessionID: "reviewer", Kind: "verification", Action: "run/1/check/base", Cwd: finished, Argv: []string{"true"}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = done.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	running, err := checkpoints.VerificationCheckout(ctx, "conv", source, "HEAD")
	if err != nil {
		t.Fatal(err)
	}
	slow, err := manager.Start(commands.Spec{SessionID: "reviewer", Kind: "verification", Action: "run/1/check/candidate", Cwd: running, Argv: []string{"sleep", "60"}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = slow.Cancel() })

	b.removeFinishedVerificationCheckouts()
	if _, err = os.Lstat(finished); !os.IsNotExist(err) {
		t.Fatalf("finished checkout kept: %v", err)
	}
	if _, err = os.Lstat(filepath.Join(running, ".git")); err != nil {
		t.Fatalf("running checkout removed: %v", err)
	}
	if err = checkpoints.RemoveVerificationCheckout(source); err == nil {
		t.Fatal("removed a path that is not a verification checkout")
	}
	if _, err = os.Lstat(filepath.Join(source, ".git")); err != nil {
		t.Fatal("source repository touched", err)
	}
}
