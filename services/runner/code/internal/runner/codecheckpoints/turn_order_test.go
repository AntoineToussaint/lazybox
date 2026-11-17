package codecheckpoints

import (
	"path/filepath"
	"testing"
	"time"
)

// A turn's end can be reported after the next turn already began: that late
// report must not close the new turn at its very start.
func TestALateTurnEndNeverClosesTheNextTurn(t *testing.T) {
	dir := newRepo(t)
	m := manager(t, Limits{})
	repos := []Repository{{Member: "app", Path: dir}}
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op-1", "one"); err != nil {
		t.Fatal(err)
	}
	ended := time.Now()
	write(t, filepath.Join(dir, "one.txt"), "one\n")
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op-2", "two"); err != nil {
		t.Fatal(err)
	}
	for _, operation := range []string{"op-1", ""} {
		if err := m.EndTurn(ctx(t), "conv", repos, operation, ended); err != nil {
			t.Fatal(err)
		}
	}
	rec, _ := m.List("conv")
	if rec.Turns[0].After == nil || rec.Turns[0].Files != 1 || rec.Turns[1].After != nil {
		t.Fatalf("turns = %+v", rec.Turns)
	}
	if err := m.EndTurn(ctx(t), "conv", repos, "op-2", time.Now()); err != nil {
		t.Fatal(err)
	}
	if rec, _ = m.List("conv"); rec.Turns[1].After == nil {
		t.Fatal("turn 2 not closed by its own end")
	}
}
