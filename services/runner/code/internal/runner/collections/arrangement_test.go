package collections

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func idsOf(list []Collection) string {
	ids := make([]string, len(list))
	for i, c := range list {
		ids[i] = c.ID
	}
	return strings.Join(ids, " ")
}

func createEmpty(t *testing.T, m *Manager, key string) Collection {
	t.Helper()
	c, err := m.Create(context.Background(), key, Spec{Name: key})
	if err != nil {
		t.Fatal(err)
	}
	// CreatedAt is in milliseconds: keep the order of creation unambiguous.
	time.Sleep(2 * time.Millisecond)
	return c
}

// The person's order is the order List answers in, and it survives a restart.
func TestArrangeKeepsThePersonsOrderAcrossRestart(t *testing.T) {
	root := t.TempDir()
	m := newTest(t, root)
	a, b, c := createEmpty(t, m, "a"), createEmpty(t, m, "b"), createEmpty(t, m, "c")
	before := idsOf(m.List())
	arranged, err := m.Arrange([]string{c.ID, a.ID, b.ID})
	if err != nil {
		t.Fatal(err)
	}
	want := strings.Join([]string{c.ID, a.ID, b.ID}, " ")
	if idsOf(arranged) != want || idsOf(m.List()) != want {
		t.Fatalf("not in the person's order: %s, want %s (by id %s)", idsOf(arranged), want, before)
	}
	if err = m.Close(); err != nil {
		t.Fatal(err)
	}
	again := newTest(t, root)
	if idsOf(again.List()) != want {
		t.Fatalf("order lost across a restart: %s, want %s", idsOf(again.List()), want)
	}
}

// A workspace created after the order was kept comes first, newest first;
// an unknown, duplicate or malformed ID is ignored; an empty order clears it.
func TestArrangePutsNewWorkspacesFirstAndIgnoresUnknown(t *testing.T) {
	m := newTest(t, t.TempDir())
	a, b := createEmpty(t, m, "a"), createEmpty(t, m, "b")
	if _, err := m.Arrange([]string{b.ID, "MISSING", a.ID, b.ID, "../escape"}); err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(m.order, " "); got != b.ID+" "+a.ID {
		t.Fatalf("kept order %q", got)
	}
	c, d := createEmpty(t, m, "c"), createEmpty(t, m, "d")
	if got, want := idsOf(m.List()), strings.Join([]string{d.ID, c.ID, b.ID, a.ID}, " "); got != want {
		t.Fatalf("new workspaces not first: %s, want %s", got, want)
	}
	// A workspace left out of a later order goes first too.
	if got, want := idsOf(mustArrange(t, m, a.ID, b.ID, c.ID)), strings.Join([]string{d.ID, a.ID, b.ID, c.ID}, " "); got != want {
		t.Fatalf("left out: %s, want %s", got, want)
	}
	cleared := mustArrange(t, m)
	if len(m.order) != 0 || idsOf(cleared) != idsOf(byID(cleared)) {
		t.Fatalf("an empty order did not go back to the Runner's: %s", idsOf(cleared))
	}
}

func TestArrangeRefusesAnOrderLongerThanTheCapacity(t *testing.T) {
	m := newTest(t, t.TempDir())
	if _, err := m.Arrange(make([]string, maxArranged+1)); !errors.Is(err, ErrArrangementTooLong) {
		t.Fatal("an order past the capacity was taken", err)
	}
}

// A damaged order file never keeps the Runner from starting: it lists by ID.
func TestUnreadableArrangementIsNoOrder(t *testing.T) {
	root := t.TempDir()
	m := newTest(t, root)
	createEmpty(t, m, "a")
	createEmpty(t, m, "b")
	if err := m.Close(); err != nil {
		t.Fatal(err)
	}
	resolved, _ := filepath.EvalSymlinks(root)
	if err := os.WriteFile(filepath.Join(resolved, ".lazybox-collections", arrangementFile), []byte("{not json"), 0600); err != nil {
		t.Fatal(err)
	}
	again := newTest(t, root)
	if list := again.List(); len(list) != 2 || idsOf(list) != idsOf(byID(list)) {
		t.Fatalf("damaged order: %s", idsOf(list))
	}
}

func mustArrange(t *testing.T, m *Manager, ids ...string) []Collection {
	t.Helper()
	list, err := m.Arrange(ids)
	if err != nil {
		t.Fatal(err)
	}
	return list
}

func byID(list []Collection) []Collection {
	out := append([]Collection(nil), list...)
	arrange(out, nil)
	return out
}
