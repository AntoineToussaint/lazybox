package work

import (
	"testing"
	"time"
)

// Leases belong to a task's own agent (or its orchestrator); an exclusive
// one refuses another task's claim on the same paths and lapses by itself;
// overlaps and holds are Lazybox's to record, once each.
func TestLeasesOverlapsAndHolds(t *testing.T) {
	s := newStore(t, t.TempDir())
	clock := time.Date(2026, 11, 16, 14, 0, 0, 0, time.UTC)
	s.Now = func() time.Time { return clock }
	m := mustOK[*State](t)(s.Create(Human, "", "Ship search", ""))
	_, grant, _ := s.IssueGrant(m.ID, Human, "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	_, a, _, _ := s.Spawn(m.ID, orchestrator("orch"), "", "", "Index", "", child("c1", PermissionAsk, "claude-default", grant))
	_, b, _, _ := s.Spawn(m.ID, orchestrator("orch"), "", "", "Rank", "", child("c2", PermissionAsk, "claude-default", grant))
	claim := func(actor Actor, task, path, mode string) error {
		_, _, err := s.ClaimLease(m.ID, actor, "", LeasePayload{Task: task, Path: path, Mode: mode})
		return err
	}
	refused(t, claim(orchestrator("c2"), a, "api/", LeaseExclusive), "only for its own task")
	refused(t, claim(orchestrator("c1"), a, "/etc/", LeaseExclusive), "relative to the workspace")
	refused(t, claim(orchestrator("c1"), a, "../x", LeaseExclusive), "relative to the workspace")
	if err := claim(orchestrator("c1"), a, "api/lexer/", LeaseExclusive); err != nil {
		t.Fatal(err)
	}
	// The orchestrator may lease for its agents' tasks.
	if err := claim(orchestrator("orch"), b, "web/", LeaseShared); err != nil {
		t.Fatal(err)
	}
	refused(t, claim(orchestrator("c2"), b, "api/lexer/scan.go", LeaseShared), "api/lexer/ is leased exclusive by “Index”")
	if err := claim(orchestrator("c2"), b, "web/app.tsx", LeaseExclusive); err != nil {
		t.Fatal("a task leases inside its own shared lease", err)
	}
	// Two hours later the exclusive lease has lapsed.
	clock = clock.Add(DefaultLeaseTTL + time.Minute)
	if err := claim(orchestrator("c2"), b, "api/lexer/scan.go", LeaseShared); err != nil {
		t.Fatal("a lapsed lease still refuses", err)
	}
	state := mustOK[*State](t)(s.Mission(m.ID))
	if live := state.LiveLeases(clock); len(live) != 1 || live[0].Path != "api/lexer/scan.go" {
		t.Fatalf("live leases: %+v", live)
	}
	if !PathMatch("api/lexer/", "api/lexer/scan.go") || PathMatch("api/lexer/", "api/lexers.go") || !PathMatch("api/a.go", "api/a.go") || PathMatch("api/a.go", "api/a.gox") {
		t.Fatal("path match")
	}
	// Overlaps: Lazybox's, once per pair and paths.
	_, _, err := s.Append(m.ID, Draft{Subject: TaskSubject(a), Type: OverlapDetected, Actor: orchestrator("orch"), Payload: OverlapPayload{Tasks: []string{a, b}, Paths: []string{"x"}, Kind: "files"}})
	refused(t, err, "cannot record overlap.detected")
	if _, wrote, err := s.RecordOverlap(m.ID, OverlapPayload{Tasks: []string{b, a}, Paths: []string{"api/shared.go"}, Kind: "files"}); err != nil || !wrote {
		t.Fatal(err, wrote)
	}
	if _, wrote, _ := s.RecordOverlap(m.ID, OverlapPayload{Tasks: []string{a, b}, Paths: []string{"api/shared.go"}, Kind: "files"}); wrote {
		t.Fatal("the same overlap recorded twice")
	}
	state = mustOK[*State](t)(s.Mission(m.ID))
	if len(state.TaskOverlaps(a)) != 1 || len(state.TaskOverlaps(b)) != 1 {
		t.Fatalf("overlaps: %+v", state.Overlaps)
	}
	// A hold is recorded once per waiting count.
	if _, wrote, _ := s.RecordHold(m.ID, HoldPayload{Grant: grant, Waiting: 3, Limit: 3}); !wrote {
		t.Fatal("hold not recorded")
	}
	if _, wrote, _ := s.RecordHold(m.ID, HoldPayload{Grant: grant, Waiting: 3, Limit: 3}); wrote {
		t.Fatal("hold recorded twice")
	}
	// A narrowed grant cannot let more work wait for review.
	if ok, _ := (Ceiling{Permission: PermissionAsk, Profiles: []string{"claude-default"}, MaxChildren: 1, ReviewLimit: 5}).Within(Ceiling{Permission: PermissionAsk, Profiles: []string{"claude-default"}, MaxChildren: 1, Depth: 1, ReviewLimit: 2}); ok {
		t.Fatal("a child got a larger review limit")
	}
}
