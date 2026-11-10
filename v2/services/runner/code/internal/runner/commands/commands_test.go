package commands

import (
	"context"
	"errors"
	"io"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/processguard"
)

func manager(t *testing.T, budget int64) *Manager {
	t.Helper()
	m, err := New(t.TempDir(), budget)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = m.Close() })
	return m
}

func TestConfiguredCommandAdmissionIsAtomicAndUncertainCleanupKeepsItsSlot(t *testing.T) {
	m, err := NewWithLimit(t.TempDir(), 1<<20, 1)
	if err != nil {
		t.Fatal(err)
	}
	defer m.Close()
	var wg sync.WaitGroup
	accepted := make(chan *Command, 16)
	for range 16 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			c, err := m.Register(Spec{SessionID: "fixture"})
			if err == nil {
				accepted <- c
			}
		}()
	}
	wg.Wait()
	close(accepted)
	if len(accepted) != 1 {
		t.Fatal("command admission exceeded capacity", len(accepted))
	}
	c := <-accepted
	if err = c.FinishPipe(nil); err != nil {
		t.Fatal(err)
	}
	c, err = m.Register(Spec{SessionID: "fixture"})
	if err != nil {
		t.Fatal("confirmed exit retained capacity", err)
	}
	c.finish(nil, "", processguard.ErrCleanupUnconfirmed)
	if _, err = m.Register(Spec{SessionID: "fixture"}); err == nil {
		t.Fatal("uncertain cleanup released capacity")
	}
	if err = c.Cancel(); !errors.Is(err, processguard.ErrCleanupUnconfirmed) {
		t.Fatal("cancel hid unresolved ownership", err)
	}
	if _, err = NewWithLimit(t.TempDir(), 1<<20, -1); err == nil {
		t.Fatal("negative command limit accepted")
	}
	defaults := manager(t, 1<<20)
	if defaults.maxLive != 128 {
		t.Fatal("zero became unlimited")
	}
}
func testContext(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	t.Cleanup(cancel)
	return ctx
}
func TestManagedExitReplayAndByteReads(t *testing.T) {
	m := manager(t, 1<<20)
	c, err := m.Start(Spec{SessionID: "execution", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", "printf alpha; printf beta >&2; exit 7"}})
	if err != nil {
		t.Fatal(err)
	}
	s, err := c.Wait(testContext(t))
	if err != nil {
		t.Fatal(err)
	}
	if s.ExitCode == nil || *s.ExitCode != 7 || s.Error != "" {
		t.Fatalf("%+v", s)
	}
	for viewer := 0; viewer < 2; viewer++ {
		var text strings.Builder
		for cursor := uint64(0); ; {
			r, err := c.Next(testContext(t), cursor)
			if err == io.EOF {
				break
			}
			if err != nil {
				t.Fatal(err)
			}
			text.Write(r.Value)
			cursor = r.Sequence
		}
		if !strings.Contains(text.String(), "output") {
			t.Fatal("no output evidence")
		}
	}
	all, _, err := c.Read(testContext(t), 0, 65536)
	if err != nil || len(all) != 9 {
		t.Fatalf("%s %v", all, err)
	}
	tail, _, err := c.Read(testContext(t), 3, 2)
	if err != nil || string(tail) != string(all[3:5]) {
		t.Fatalf("%s %v", tail, err)
	}
	if _, _, err = c.Read(testContext(t), 10, 1); err == nil {
		t.Fatal("accepted missing byte range")
	}
}
func TestPipeEOFIsNotExitAndBudgetFailureIsVisible(t *testing.T) {
	m := manager(t, 4096)
	c, err := m.Register(Spec{SessionID: "execution", Title: "pipe"})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Publish(0, []byte("hello")); err != nil {
		t.Fatal(err)
	}
	if err = c.Publish(0, []byte("hello")); err == nil {
		t.Fatal("duplicate offset appended")
	}
	if err = c.Publish(5, []byte(strings.Repeat("x", 4096))); err == nil {
		t.Fatal("capacity not enforced")
	}
	if c.Status().Error == "" {
		t.Fatal("capture failure hidden")
	}
	_ = c.FinishPipe(errors.New("publication failed"))
	s, err := c.Wait(testContext(t))
	if err != nil || !s.Done || s.ExitCode != nil || s.Error == "" {
		t.Fatalf("%+v %v", s, err)
	}
	if _, err = c.Next(testContext(t), c.spool.Count()); err == nil {
		t.Fatal("failed capture appeared complete")
	}
}
func TestCancelOwnsProcessGroupAndFailedStartIsObserved(t *testing.T) {
	m := manager(t, 1<<20)
	c, err := m.Start(Spec{SessionID: "execution", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", "printf ready; sleep 60 & wait"}})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Cancel(); err != nil {
		t.Fatal(err)
	}
	s, err := c.Wait(testContext(t))
	if err != nil || s.Signal == "" || !s.Done {
		t.Fatalf("%+v %v", s, err)
	}
	c, err = m.Start(Spec{SessionID: "execution", Cwd: t.TempDir(), Argv: []string{"/nonexistent/lazybox-test-executable"}})
	if err != nil {
		t.Fatal(err)
	}
	s, err = c.Wait(testContext(t))
	if err != nil || !s.Done || s.Error == "" || s.ExitCode != nil {
		t.Fatalf("%+v %v", s, err)
	}
}
