package processguard

import (
	"bytes"
	"os/exec"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
)

// A graceful stop reaches the whole group with SIGTERM, lets the program run
// its own shutdown, and reports the program's own exit.
func TestInterruptSendsSIGTERMToTheGroupAndLetsTheProgramExit(t *testing.T) {
	var out syncBuffer
	// Short sleeps: a shell runs its trap only between commands, so a SIGTERM
	// that lands before a sleep starts must not wait for that sleep to end.
	cmd := exec.Command("/bin/sh", "-c", `trap 'echo graceful; exit 7' TERM; echo ready; while :; do sleep 0.05; done`)
	cmd.Stdout = &out
	g, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = g.Stop(); _, _ = g.Wait() })
	waitOutput(t, &out, "ready")
	if err = g.Interrupt(); err != nil {
		t.Fatal(err)
	}
	done := make(chan Outcome, 1)
	go func() { o, _ := g.Wait(); done <- o }()
	select {
	case o := <-done:
		if o.Cancelled || o.ExitCode != 7 {
			t.Fatalf("the program's own exit was not reported: %+v", o)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("SIGTERM did not stop the program")
	}
	if !strings.Contains(out.String(), "graceful") {
		t.Fatalf("the program's shutdown did not run: %q", out.String())
	}
}

// A program that ignores SIGTERM still stops once its owner stops it.
func TestInterruptIgnoredStillStopsOnStop(t *testing.T) {
	var out syncBuffer
	cmd := exec.Command("/bin/sh", "-c", `trap '' TERM; echo ready; while :; do sleep 1; done`)
	cmd.Stdout = &out
	g, err := Start(cmd)
	if err != nil {
		t.Fatal(err)
	}
	waitOutput(t, &out, "ready")
	if err = g.Interrupt(); err != nil {
		t.Fatal(err)
	}
	done := make(chan Outcome, 1)
	go func() { o, _ := g.Wait(); done <- o }()
	select {
	case o := <-done:
		t.Fatalf("an ignored SIGTERM ended the program: %+v", o)
	case <-time.After(500 * time.Millisecond):
	}
	if err = g.Stop(); err != nil {
		t.Fatal(err)
	}
	select {
	case o := <-done:
		if !o.Cancelled || o.Signal != syscall.SIGKILL.String() {
			t.Fatalf("Stop after Interrupt did not kill: %+v", o)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("Stop did not end a program that ignores SIGTERM")
	}
	if g.Interrupt() == nil {
		t.Fatal("Interrupt after Stop must report that the process is stopping")
	}
}

// syncBuffer is read by the test while the guard copies output into it.
type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *syncBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

func (b *syncBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.String()
}

func waitOutput(t *testing.T, out *syncBuffer, marker string) {
	t.Helper()
	for deadline := time.Now().Add(10 * time.Second); time.Now().Before(deadline); time.Sleep(10 * time.Millisecond) {
		if strings.Contains(out.String(), marker) {
			return
		}
	}
	t.Fatalf("no %q in %q", marker, out.String())
}
