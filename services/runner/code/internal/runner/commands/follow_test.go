package commands

import (
	"context"
	"strings"
	"testing"
	"time"
)

// A followed pipe (a `lazybox run`, foreground or background) outlives a
// Runner restart: the next Runner reopens it where its output stopped, the run
// keeps publishing, and a stop still reaches it. An unfollowed pipe ends with
// the Runner, as before.
func TestAFollowedPipeOutlivesARunnerRestart(t *testing.T) {
	dir := t.TempDir()
	m, err := New(dir, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	followed, err := m.Register(Spec{SessionID: "fixture", Title: "npm run dev"})
	if err != nil {
		t.Fatal(err)
	}
	plain, err := m.Register(Spec{SessionID: "fixture", Title: "cat"})
	if err != nil {
		t.Fatal(err)
	}
	if err = followed.Publish(0, []byte("ready\n")); err != nil {
		t.Fatal(err)
	}
	if _, err = followed.Follow(context.Background(), FollowUpdate{Background: true, PID: 77, Ports: []uint32{5173}}, 0); err != nil {
		t.Fatal(err)
	}
	if err = m.Detach(); err != nil {
		t.Fatal(err)
	}

	next, err := New(dir, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	c, err := next.Get(followed.Status().ID)
	if err != nil {
		t.Fatal(err)
	}
	state := c.Status()
	if state.Done || !state.Background || !state.Following || state.PID != 77 || state.Offset != 6 || len(state.Ports) != 1 {
		t.Fatalf("the followed pipe was not reopened as it was: %+v", state)
	}
	if err = c.Publish(6, []byte("still serving\n")); err != nil {
		t.Fatal("the run cannot publish after the restart", err)
	}
	if err = c.Cancel(); err != nil {
		t.Fatal(err)
	}
	reply, err := c.Follow(context.Background(), FollowUpdate{Background: true, PID: 77}, time.Second)
	if err != nil || !reply.Stop || reply.Offset != 20 {
		t.Fatalf("stop after restart: %+v %v", reply, err)
	}
	gone, err := next.Get(plain.Status().ID)
	if err != nil || !gone.Status().Done {
		t.Fatalf("an unfollowed pipe ends with the Runner: %+v %v", gone.Status(), err)
	}
}

// A followed pipe whose `lazybox run` went silent (killed, say) is ended, so
// it does not read "running" forever.
func TestASilentFollowedPipeIsEnded(t *testing.T) {
	saved := followSilence
	followSilence = 300 * time.Millisecond
	defer func() { followSilence = saved }()
	m := manager(t, 1<<20)
	c, err := m.Register(Spec{SessionID: "fixture", Title: "npm run dev"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = c.Follow(context.Background(), FollowUpdate{Background: true}, 0); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	state, err := c.Wait(ctx)
	if err != nil || !state.Done || !strings.Contains(state.Error, "stopped reporting") {
		t.Fatalf("%+v %v", state, err)
	}
}

// Moving to the background is for a foreground run the Runner can reach.
func TestRequestBackgroundNeedsAFollowedForegroundRun(t *testing.T) {
	m := manager(t, 1<<20)
	c, err := m.Register(Spec{SessionID: "fixture", Title: "go test"})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.RequestBackground(); err == nil {
		t.Fatal("an unfollowed pipe moved")
	}
	if err = c.Cancel(); err == nil {
		t.Fatal("an unfollowed pipe has no process to stop")
	}
	if _, err = c.Follow(context.Background(), FollowUpdate{}, 0); err != nil {
		t.Fatal(err)
	}
	if err = c.RequestBackground(); err != nil {
		t.Fatal(err)
	}
	if reply, _ := c.Follow(context.Background(), FollowUpdate{}, 0); !reply.Background {
		t.Fatal("the request was not delivered")
	}
	if reply, _ := c.Follow(context.Background(), FollowUpdate{Background: true}, 0); reply.Background {
		t.Fatal("a run in the background is not asked again")
	}
	if err = c.RequestBackground(); err == nil {
		t.Fatal("a background run moved again")
	}
}
