package processguard

import (
	"errors"
	"os"
	"os/exec"
	"syscall"
	"testing"
	"time"
)

// A released guard hands its helper over: it no longer stops it or reads its
// outcome, and the new owner (Adopt) stops it gracefully and reads the exit.
func TestReleaseHandsTheHelperToAnAdoptingOwner(t *testing.T) {
	out, err := os.CreateTemp(t.TempDir(), "out")
	if err != nil {
		t.Fatal(err)
	}
	defer out.Close()
	cmd := exec.Command("/bin/sleep", "30")
	cmd.Stdout, cmd.Stderr = out, out
	g, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	pid := g.PID()
	if pid <= 0 {
		t.Fatal("no helper pid")
	}
	live, result, err := g.Release()
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err = g.Release(); err == nil {
		t.Fatal("released twice")
	}
	if err = g.Stop(); err != nil {
		t.Fatal(err)
	}
	time.Sleep(100 * time.Millisecond)
	if syscall.Kill(-pid, 0) != nil {
		t.Fatal("Stop on a released guard stopped the program")
	}
	adopted := Adopt(live, result, pid)
	if adopted.PID() != pid {
		t.Fatal("adopted pid")
	}
	if err = adopted.Interrupt(); err != nil {
		t.Fatal(err)
	}
	done := make(chan Outcome, 1)
	go func() { o, _ := adopted.Wait(); done <- o }()
	select {
	case o := <-done:
		if !o.Started || o.Signal == "" {
			t.Fatalf("the adopted owner reads the program's exit: %+v", o)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("the program did not stop on SIGTERM")
	}
	if _, err = g.Wait(); !errors.Is(err, ErrReleased) {
		t.Fatal("the original owner must not read the outcome", err)
	}
}

// A guard that copies its program's output itself cannot be handed over.
func TestReleaseRefusesAGuardThatCopiesOutput(t *testing.T) {
	cmd := exec.Command("/bin/sleep", "30")
	cmd.Stdout = &nopWriter{}
	g, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = g.Stop(); _, _ = g.Wait() }()
	if _, _, err = g.Release(); err == nil {
		t.Fatal("released a guard that copies output")
	}
}

type nopWriter struct{}

func (nopWriter) Write(p []byte) (int, error) { return len(p), nil }
