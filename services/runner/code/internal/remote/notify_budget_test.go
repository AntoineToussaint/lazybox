package remote

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
)

// A test notification spends the phone's per-minute budget like any other,
// so repeated tests cannot flood the phone.
func TestTestPushesSpendThePhonesBudget(t *testing.T) {
	f := newNotifier(t)
	d := f.phone(t, apple, nil)
	sent := 0
	var err error
	for ; sent < 20; sent++ {
		if err = f.n.Test(context.Background(), d.ID); err != nil {
			break
		}
	}
	if !errors.Is(err, devices.ErrPushBudget) || sent != perDeviceBudget {
		t.Fatalf("after %d tests: %v", sent, err)
	}
	if got := len(f.sender.take()); got != perDeviceBudget {
		t.Fatalf("%d pushes reached the push service", got)
	}
	f.now = f.now.Add(time.Minute)
	if err = f.n.Test(context.Background(), d.ID); err != nil {
		t.Fatalf("a minute later: %v", err)
	}
}
