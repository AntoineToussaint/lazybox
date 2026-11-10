package processguard

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
)

func TestGuardOwnerFixture(t *testing.T) {
	root := os.Getenv("LAZYBOX_GUARD_OWNER_FIXTURE")
	if root == "" {
		return
	}
	cmd := exec.Command("/bin/sh", "-c", `sleep 60 & printf '%s %s\n' "$$" "$!" > "$1"; wait`, "fixture", filepath.Join(root, "children"))
	guard, err := Start(cmd)
	if err != nil {
		os.Exit(91)
	}
	if err = os.WriteFile(filepath.Join(root, "group"), []byte(strconv.Itoa(cmd.Process.Pid)), 0600); err != nil {
		os.Exit(92)
	}
	_, _ = guard.Wait()
	os.Exit(0)
}
func waitFile(t *testing.T, path string) string {
	t.Helper()
	until := time.Now().Add(10 * time.Second)
	for time.Now().Before(until) {
		raw, err := os.ReadFile(path)
		if err == nil && len(strings.Fields(string(raw))) > 0 {
			return strings.TrimSpace(string(raw))
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("fixture did not become ready", path)
	return ""
}
func assertStopped(t *testing.T, pid int) {
	t.Helper()
	until := time.Now().Add(5 * time.Second)
	for time.Now().Before(until) {
		if err := syscall.Kill(pid, 0); err == syscall.ESRCH {
			return
		}
		// An orphan zombie has no executable resources; PID 1 owns its reaping.
		state, _ := exec.Command("ps", "-o", "stat=", "-p", strconv.Itoa(pid)).Output()
		if strings.HasPrefix(strings.TrimSpace(string(state)), "Z") {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("owned process %d survived Runner death", pid)
}
func TestRunnerSIGKILLReclaimsOwnedGroupWithoutTouchingOtherProcesses(t *testing.T) {
	binary, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	root := t.TempDir()
	owner := exec.Command(binary, "-test.run=^TestGuardOwnerFixture$")
	owner.Env = append(os.Environ(), "LAZYBOX_GUARD_OWNER_FIXTURE="+root)
	if err = owner.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = owner.Process.Kill(); _ = owner.Wait() })
	group, err := strconv.Atoi(waitFile(t, filepath.Join(root, "group")))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = syscall.Kill(-group, syscall.SIGKILL) })
	var child, grandchild int
	if _, err = fmt.Sscan(waitFile(t, filepath.Join(root, "children")), &child, &grandchild); err != nil {
		t.Fatal(err)
	}
	unrelated := exec.Command("/bin/sleep", "60")
	if err = unrelated.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = unrelated.Process.Kill(); _ = unrelated.Wait() })
	if err = owner.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	_ = owner.Wait()
	for _, pid := range []int{group, child, grandchild} {
		assertStopped(t, pid)
	}
	if err = unrelated.Process.Signal(syscall.Signal(0)); err != nil {
		t.Fatal("unrelated process was terminated", err)
	}
}
func TestNormalExitPreservesStatusAndReclaimsBackgroundDescendant(t *testing.T) {
	path := filepath.Join(t.TempDir(), "child")
	cmd := exec.Command("/bin/sh", "-c", `sleep 60 & printf '%s\n' "$!" > "$1"; exit 7`, "fixture", path)
	guard, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	outcome, err := guard.Wait()
	if err != nil || outcome.ExitCode != 7 || !outcome.Started {
		t.Fatal("lost target exit status", outcome, err)
	}
	pid, err := strconv.Atoi(waitFile(t, path))
	if err != nil {
		t.Fatal(err)
	}
	assertStopped(t, pid)
}
func TestFailedTargetStartIsNotAProgramExit(t *testing.T) {
	cmd := exec.Command("/nonexistent/lazybox-guard-fixture")
	guard, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	outcome, err := guard.Wait()
	if err == nil || outcome.Started {
		t.Fatal("failed start claimed an executed program", outcome, err)
	}
}

func TestStopAndWaitAreIdempotentAndPreserveCancellationEvidence(t *testing.T) {
	cmd := exec.Command("/bin/sleep", "60")
	g, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = g.Stop(); _, _ = g.Wait() })
	var wg sync.WaitGroup
	for range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if err := g.Stop(); err != nil {
				t.Error(err)
			}
			outcome, err := g.Wait()
			if err != nil || !outcome.Cancelled || outcome.Signal != syscall.SIGKILL.String() {
				t.Errorf("missing cleanup result: %+v %v", outcome, err)
			}
		}()
	}
	wg.Wait()
}

func TestGuardPreservesArgvDirectoryEnvironmentAndOutput(t *testing.T) {
	root := t.TempDir()
	cmd := exec.Command("/bin/sh", "-c", `printf '%s\n' "$PWD" "$FIXTURE" "$1"; printf 'stderr' >&2`, "fixture", "space $literal; argument")
	cmd.Dir = root
	cmd.Env = []string{"FIXTURE=exact-value"}
	var out, diagnostic bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &diagnostic
	g, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	result, err := g.Wait()
	canonical, pathErr := filepath.EvalSymlinks(root)
	if pathErr != nil {
		t.Fatal(pathErr)
	}
	if err != nil || result.ExitCode != 0 || out.String() != canonical+"\nexact-value\nspace $literal; argument\n" || diagnostic.String() != "stderr" {
		t.Fatalf("lost command contract: %+v %v %q %q", result, err, out.String(), diagnostic.String())
	}
}

func TestGuardRejectsUnsupportedCommandsAndUnownedHelperInvocation(t *testing.T) {
	for _, cmd := range []*exec.Cmd{nil, {}, {Args: []string{"target"}, SysProcAttr: &syscall.SysProcAttr{Setpgid: true}}, exec.CommandContext(context.Background(), "/bin/sleep", "60")} {
		if _, err := Start(cmd); err == nil {
			t.Fatal("accepted unsupported command")
		}
	}
	binary, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(binary, helperArgument, "/bin/echo", "must-not-run")
	out, err := cmd.CombinedOutput()
	if err == nil || cmd.ProcessState.ExitCode() != 125 || len(out) != 0 {
		t.Fatalf("unowned invocation: %q %v", out, err)
	}
}

func TestDetachedOutputFixture(t *testing.T) {
	path := os.Getenv("LAZYBOX_DETACHED_OUTPUT_FIXTURE")
	if path == "" {
		return
	}
	child := exec.Command("/bin/sleep", "60")
	child.Stdout, child.Stderr = os.Stdout, os.Stderr
	child.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err := child.Start(); err != nil {
		os.Exit(91)
	}
	if err := os.WriteFile(path, []byte(strconv.Itoa(child.Process.Pid)), 0600); err != nil {
		_ = child.Process.Kill()
		os.Exit(92)
	}
	os.Exit(0)
}

func TestUnclosedOutputCannotClaimCompleteCleanup(t *testing.T) {
	binary, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(t.TempDir(), "detached-child")
	cmd := exec.Command(binary, "-test.run=^TestDetachedOutputFixture$")
	cmd.Env = append(os.Environ(), "LAZYBOX_DETACHED_OUTPUT_FIXTURE="+path)
	var output bytes.Buffer
	cmd.Stdout = &output
	cmd.WaitDelay = 50 * time.Millisecond
	g, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(waitFile(t, path))
	if err != nil {
		t.Fatal(err)
	}
	// This fixture deliberately escapes the supported group boundary. It must
	// be reclaimed explicitly by the test, never mistaken for a contained child.
	t.Cleanup(func() { _ = syscall.Kill(pid, syscall.SIGKILL); assertStopped(t, pid) })
	_, err = g.Wait()
	if err == nil {
		t.Fatal("unclosed output was falsely reported as complete cleanup")
	}
}

// A cancellation that arrives while the helper is still starting its program
// must not race the start: either the program never starts, or it is in the
// group when the group is killed. A child forked after the kill sweep would
// survive, keep the output pipe open and leave cleanup unconfirmed.
func TestCancelDuringProgramStartLeavesNoSurvivor(t *testing.T) {
	marker := "lazybox-guard-start-race-" + strconv.Itoa(os.Getpid())
	for i := 0; i < 40; i++ {
		var out bytes.Buffer
		cmd := exec.Command("/bin/sh", "-c", "sleep 60 & wait", marker)
		cmd.Stdout = &out
		g, err := Start(cmd)
		if err != nil {
			t.Fatal(err)
		}
		_ = g.Stop()
		if _, err = g.Wait(); err != nil {
			t.Fatalf("iteration %d: cleanup unconfirmed after an immediate cancel: %v", i, err)
		}
	}
	survivors, _ := exec.Command("/usr/bin/pgrep", "-f", marker).Output()
	if len(bytes.TrimSpace(survivors)) != 0 {
		_ = exec.Command("/usr/bin/pkill", "-f", marker).Run()
		t.Fatalf("owned programs survived cancellation: %s", survivors)
	}
}
