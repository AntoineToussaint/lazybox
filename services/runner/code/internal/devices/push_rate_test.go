package devices_test

import (
	"errors"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
)

// Each change rewrites the paired devices file: a phone changes its
// notifications at most pushChangesPerMinute times a minute, and another
// phone is not held back by it.
func TestPushChangesAreRateLimitedPerPhone(t *testing.T) {
	m, _ := open(t, t.TempDir())
	now := time.Date(2026, 11, 17, 9, 0, 0, 0, time.UTC)
	m.Now = func() time.Time { return now }
	d := pair(t, m, phone(t))
	other := pair(t, m, phone(t))
	var err error
	changes := 0
	for ; changes < 100 && err == nil; changes++ {
		_, err = m.SetPush(d.ID, devices.PushChange{Unsubscribe: true})
	}
	if !errors.Is(err, devices.ErrPushChanges) || changes < 10 || changes > 40 {
		t.Fatalf("after %d changes: %v", changes, err)
	}
	if _, err = m.SetPush(other.ID, devices.PushChange{Unsubscribe: true}); err != nil {
		t.Fatalf("another phone: %v", err)
	}
	now = now.Add(time.Minute)
	if _, err = m.SetPush(d.ID, devices.PushChange{Unsubscribe: true}); err != nil {
		t.Fatalf("a minute later: %v", err)
	}
}
