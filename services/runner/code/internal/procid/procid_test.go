package procid

import (
	"errors"
	"os"
	"os/exec"
	"syscall"
	"testing"
	"time"
)

func TestIdentityOfARunningProcessIsAlive(t *testing.T) {
	self, err := Of(os.Getpid())
	if err != nil || self.Start == 0 {
		t.Fatalf("own identity = %+v, %v", self, err)
	}
	again, _ := Of(os.Getpid())
	if again != self {
		t.Fatalf("identity changed between reads: %+v then %+v", self, again)
	}
	if state, err := self.Check(); err != nil || state != Alive {
		t.Fatalf("Check = %v, %v; want alive", state, err)
	}
}

// A PID that now belongs to another program is not the recorded process: it
// is reported recycled, and never signalled.
func TestARecycledPIDIsNeverSignalled(t *testing.T) {
	other := exec.Command("sleep", "30")
	if err := other.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = other.Process.Kill(); _ = other.Wait() })
	live, err := Of(other.Process.Pid)
	if err != nil {
		t.Fatal(err)
	}
	// The record of an earlier process that had the same number.
	recorded := Identity{PID: live.PID, Start: live.Start - 5_000_000}
	if state, err := recorded.Check(); err != nil || state != Recycled {
		t.Fatalf("Check = %v, %v; want recycled", state, err)
	}
	if err := recorded.Signal(syscall.SIGKILL, false); !errors.Is(err, ErrNotSame) {
		t.Fatalf("Signal on a recycled PID = %v; want ErrNotSame", err)
	}
	// No start recorded: not provably the same process, so not signalled either.
	if err := (Identity{PID: live.PID}).Signal(syscall.SIGKILL, false); !errors.Is(err, ErrNotSame) {
		t.Fatalf("Signal without a start time = %v; want ErrNotSame", err)
	}
	if state, _ := live.Check(); state != Alive {
		t.Fatalf("the other program was hit: %v", state)
	}
	if err := live.Signal(syscall.SIGKILL, false); err != nil {
		t.Fatalf("Signal on the recorded process: %v", err)
	}
	_ = other.Wait()
	if state, _ := live.Check(); state != Gone {
		t.Fatalf("after its exit Check = %v; want gone", state)
	}
}

func TestBootedIsInThePast(t *testing.T) {
	boot, err := Booted()
	if err != nil {
		t.Fatal(err)
	}
	if !boot.Before(time.Now()) || time.Since(boot) > 10*365*24*time.Hour {
		t.Fatalf("boot time %v is not plausible", boot)
	}
}
