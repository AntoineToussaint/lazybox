package sessionhost

import (
	"context"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

// A host starts in a transient scope of its own, named after it, where the
// user manager offers one; without one it starts as before.
func TestAHostStartsInItsOwnScope(t *testing.T) {
	if status := Scopes(); !status.Scoped {
		t.Skip("no systemd user manager here: " + status.Reason)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	root := t.TempDir()
	c, err := Launch(ctx, root, Options{Kind: "command", Label: "scope check"})
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = Stop(context.Background(), root, c.ID()) }()
	d, err := Describe(root, c.ID())
	if err != nil {
		t.Fatal(err)
	}
	cgroup, err := os.ReadFile("/proc/" + strconv.Itoa(d.PID) + "/cgroup")
	if err != nil {
		t.Fatal(err)
	}
	if want := "/" + ScopeUnit(c.ID()); !strings.Contains(string(cgroup), want) {
		t.Fatalf("host %d is in %s, not %s", d.PID, cgroup, want)
	}
	// The scope keeps the host's own session: no hangup reaches it either.
	if sid, err := unix.Getsid(d.PID); err != nil || sid != d.PID {
		t.Fatalf("host session %d %v, want its own (%d)", sid, err, d.PID)
	}
	_ = c.Detach()

	disableScopes = true
	defer func() { disableScopes = false }()
	plain, err := Launch(ctx, root, Options{Kind: "command"})
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = Stop(context.Background(), root, plain.ID()) }()
	d, _ = Describe(root, plain.ID())
	cgroup, _ = os.ReadFile("/proc/" + strconv.Itoa(d.PID) + "/cgroup")
	if strings.Contains(string(cgroup), "lazybox-host-") {
		t.Fatalf("a host started without scopes is in one: %s", cgroup)
	}
	_ = plain.Detach()
}
