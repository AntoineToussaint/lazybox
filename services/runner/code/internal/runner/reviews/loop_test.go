package reviews

import (
	"encoding/json"
	"errors"
	"os"
	"strings"
	"testing"
)

// The tray (review-model.ts reviewMessage) and the loop send the same text.
func TestFindingsMessageMatchesTheTray(t *testing.T) {
	raw, err := os.ReadFile("testdata/findings-message.json")
	if err != nil {
		t.Fatal(err)
	}
	var golden struct {
		Comments []struct {
			Comment
			Freshness                        Freshness `json:"freshness"`
			CurrentStartLine, CurrentEndLine int
		} `json:"comments"`
		Message string `json:"message"`
	}
	if err = json.Unmarshal(raw, &golden); err != nil {
		t.Fatal(err)
	}
	var in []Anchored
	for _, c := range golden.Comments {
		in = append(in, Anchored{Comment: c.Comment, Anchor: Anchor{Freshness: c.Freshness, StartLine: c.CurrentStartLine, EndLine: c.CurrentEndLine}})
	}
	if got := FindingsMessage(in); got != golden.Message {
		t.Fatalf("the Runner's findings message differs from the tray's:\n--- runner\n%s\n--- tray\n%s", got, golden.Message)
	}
}

// loopFixture: a review in a store, its first round under way.
type loopFixture struct {
	t     *testing.T
	s     *Store
	dir   string
	run   Run
	clock int64
}

func newLoop(t *testing.T, mode string, rounds int) *loopFixture {
	t.Helper()
	f := &loopFixture{t: t, dir: t.TempDir(), clock: 1000}
	var err error
	if f.s, err = New(f.dir); err != nil {
		t.Fatal(err)
	}
	if f.run, err = f.s.CreateRun("w", Run{ReviewerSession: "e-rev", ReviewerLabel: "Codex", SourceSession: "e-auth", SourceLabel: "Claude", ScopeLabel: "Turn 1"}); err != nil {
		t.Fatal(err)
	}
	f.update(func(r *Review) error {
		run, _ := findRun(r, f.run.ID)
		StartLoop(run, mode, rounds, f.now())
		return nil
	})
	return f
}

func (f *loopFixture) now() int64 { f.clock += 10; return f.clock }

func (f *loopFixture) update(change func(*Review) error) Review {
	f.t.Helper()
	rv, err := f.s.Update("w", change)
	if err != nil {
		f.t.Fatal(err)
	}
	for _, r := range rv.Runs {
		if r.ID == f.run.ID {
			f.run = r
		}
	}
	return rv
}

func (f *loopFixture) file(key, severity string) Comment {
	f.t.Helper()
	c, _, err := f.s.AddFinding("w", Comment{Member: "api", Path: "a.go", StartLine: 1, Snippet: []string{"x"}, Author: AuthorAgent, AuthorLabel: "Codex", Severity: severity, Title: key, ReviewID: f.run.ID, Key: key})
	if err != nil {
		f.t.Fatal(err)
	}
	return c
}

func (f *loopFixture) roundEnded() Action {
	var a Action
	f.update(func(r *Review) (err error) { a, err = RoundEnded(r, f.run.ID, f.now()); return })
	return a
}

func (f *loopFixture) send(ids ...string) {
	f.update(func(r *Review) error {
		for i := range r.Comments {
			if containsID(ids, r.Comments[i].ID) {
				r.Comments[i].Disposition = Sent
			}
		}
		FindingsSent(r, f.run.ID, ids, "You", "Claude", f.now())
		return nil
	})
}

func (f *loopFixture) authorTurn(turn, files int, startedAt int64) Action {
	var a Action
	f.update(func(r *Review) (err error) {
		a, err = AuthorTurnEnded(r, f.run.ID, turn, startedAt, files, f.now())
		return
	})
	return a
}

func (f *loopFixture) verdict(id, state, note string) {
	f.update(func(r *Review) error { _, err := Verdict(r, f.run.ID, id, state, note, f.now()); return err })
}

func kinds(r Run) string {
	var out []string
	for _, e := range r.Timeline {
		out = append(out, e.Kind)
	}
	return strings.Join(out, " ")
}

// The loop iterates on blockers: it ends with majors still open, but never
// calls that clean, and says how many are left. Minors and nits stay clean.
func TestLoopEndingWithMajorsOpenIsNotClean(t *testing.T) {
	f := newLoop(t, LoopAsk, 3)
	f.file("logged", "major")
	f.file("naming", "minor")
	if a := f.roundEnded(); a.Escalate != "" || f.run.StopReason != StopClean || f.run.Detail != "No blocker open after 1 round; 1 major still open." {
		t.Fatalf("majors open: %+v %+v", a, f.run)
	}
	g := newLoop(t, LoopAsk, 3)
	g.file("naming", "minor")
	if g.roundEnded(); g.run.Detail != "Clean: no blocker open after 1 round." {
		t.Fatalf("minors only: %+v", g.run)
	}
}

// Two rounds: a blocker is sent, the author fixes it, the reviewer resolves
// it, the loop ends clean. Turns that do not count are ignored on the way.
func TestLoopEndsCleanWhenTheReviewerResolvesTheBlocker(t *testing.T) {
	f := newLoop(t, LoopAsk, 3)
	if f.run.Round != 1 || f.run.Phase != PhaseReviewing || f.run.MaxRounds != 3 {
		t.Fatalf("start: %+v", f.run)
	}
	blocker := f.file("logged", "blocker")
	nit := f.file("name", "nit")
	if blocker.Round != 1 {
		t.Fatalf("filed in round %d", blocker.Round)
	}
	before := f.clock
	if a := f.roundEnded(); a.Rereview || a.SendFindings != nil || a.Escalate != "" || f.run.Phase != PhaseTriage {
		t.Fatalf("ask mode waits for the person: %+v %+v", a, f.run)
	}
	// A turn of the author's before the findings were sent changes nothing.
	if a := f.authorTurn(1, 3, before); a.Rereview || f.run.Phase != PhaseTriage {
		t.Fatal("a turn before the findings were sent started a round")
	}
	f.send(blocker.ID)
	if f.run.Phase != PhaseFixing || f.run.FixingSince == 0 {
		t.Fatalf("after sending: %+v", f.run)
	}
	if a := f.authorTurn(2, 0, f.clock); a.Rereview || f.run.Round != 1 {
		t.Fatal("a turn without changes started a round")
	}
	if a := f.authorTurn(1, 4, before); a.Rereview {
		t.Fatal("a turn that began before the findings were sent started a round")
	}
	if a := f.authorTurn(3, 2, f.clock); !a.Rereview || f.run.Round != 2 || f.run.Phase != PhaseReviewing || f.run.Rounds[1].AuthorTurn != 3 || f.run.State != RunRunning {
		t.Fatalf("the author's fix starts round 2: %+v %+v", a, f.run)
	}
	f.verdict(blocker.ID, "resolved", "returns the error now")
	if a := f.roundEnded(); a.Escalate != "" || f.run.StopReason != StopClean || f.run.Phase != PhaseDone {
		t.Fatalf("clean: %+v %+v", a, f.run)
	}
	if r := f.run.Rounds[1]; r.Resolved != 1 || r.Blockers != 0 {
		t.Fatalf("round 2: %+v", r)
	}
	rv, _ := f.s.Load("w")
	for _, c := range rv.Comments {
		if c.ID == blocker.ID && (c.Disposition != Resolved || !strings.Contains(c.Note, "Resolved by Codex: returns the error now")) {
			t.Fatalf("resolved finding: %+v", c)
		}
		if c.ID == nit.ID && c.StillOpenRounds != 0 {
			t.Fatal("a finding never sent counted as still open")
		}
	}
	// Sent, then waiting; a turn without changes noted; the fix; the verdict; clean.
	if got := kinds(f.run); got != "round_started filed filed round_ended triage sent fixing fixing round_started resolved round_ended stopped" {
		t.Fatalf("timeline: %s", got)
	}
	// A loop that ended starts nothing more.
	if a := f.authorTurn(4, 5, f.clock); a.Rereview {
		t.Fatal("an ended loop started a round")
	}
}

// The same blocker still open after two fixes: the loop stops and asks the
// person, whether the reviewer reopened it or said nothing about it.
func TestLoopStopsWhenTheSameBlockerIsStillOpenTwice(t *testing.T) {
	f := newLoop(t, LoopAsk, 5)
	blocker := f.file("logged", "blocker")
	f.roundEnded()
	f.send(blocker.ID)
	f.authorTurn(2, 1, f.clock)
	f.verdict(blocker.ID, "open", "still logs")
	if a := f.roundEnded(); a.Escalate != "" || f.run.Phase != PhaseTriage {
		t.Fatalf("once still open, the loop goes on: %+v %+v", a, f.run)
	}
	rv, _ := f.s.Load("w")
	if c := rv.Comments[0]; c.Disposition != Open || !c.Delivered || c.StillOpenRounds != 1 || c.SentTo != "" {
		t.Fatalf("reopened: %+v", c)
	}
	f.send(blocker.ID)
	f.authorTurn(3, 1, f.clock)
	// Silence is not a disposition: not resolved is still open.
	a := f.roundEnded()
	if f.run.StopReason != StopStuck || !strings.Contains(a.Escalate, "“logged”") || !strings.Contains(a.Escalate, "still open after two fixes") {
		t.Fatalf("stuck: %+v %+v", a, f.run)
	}
	if !strings.HasSuffix(kinds(f.run), "escalated") {
		t.Fatal(kinds(f.run))
	}
}

func TestLoopStopsAtItsRoundLimitAndAsksThePerson(t *testing.T) {
	f := newLoop(t, LoopAsk, 2)
	blocker := f.file("logged", "blocker")
	f.roundEnded()
	f.send(blocker.ID)
	f.authorTurn(2, 1, f.clock)
	other := f.file("new-race", "blocker")
	if other.Round != 2 {
		t.Fatalf("filed in round %d", other.Round)
	}
	f.verdict(blocker.ID, "resolved", "")
	if a := f.roundEnded(); f.run.StopReason != StopMaxRounds || !strings.Contains(a.Escalate, "1 blocker still open after 2 rounds") {
		t.Fatalf("max rounds: %+v %+v", a, f.run)
	}
}

// Auto: Lazybox sends the findings (accepting proposed ones); paused, it
// sends nothing and re-reviews nothing until resumed; off is one pass.
func TestLoopAutoPauseAndOff(t *testing.T) {
	f := newLoop(t, LoopAuto, 3)
	a1, a2 := f.file("logged", "blocker"), f.file("name", "minor")
	if a := f.roundEnded(); len(a.SendFindings) != 2 || !containsID(a.SendFindings, a1.ID) || !containsID(a.SendFindings, a2.ID) {
		t.Fatalf("auto sends every live finding: %+v", a)
	}
	f.send(a1.ID, a2.ID)
	f.update(func(r *Review) error { return Control(r, f.run.ID, ControlPause, "", 0, f.now()) })
	if a := f.authorTurn(2, 3, f.clock); a.Rereview || !f.run.Paused {
		t.Fatal("a paused loop re-reviewed")
	}
	f.update(func(r *Review) error { return Control(r, f.run.ID, ControlResume, "", 0, f.now()) })
	if a := f.authorTurn(3, 1, f.clock); !a.Rereview {
		t.Fatal("a resumed loop did not re-review")
	}
	f.update(func(r *Review) error { return Control(r, f.run.ID, ControlLoop, "ask", 7, f.now()) })
	if f.run.Loop != LoopAsk || f.run.MaxRounds != 7 {
		t.Fatalf("mode: %+v", f.run)
	}
	f.update(func(r *Review) error { return Control(r, f.run.ID, ControlTakeOver, "", 0, f.now()) })
	if f.run.StopReason != StopTakenOver || f.run.Active() {
		t.Fatalf("take over: %+v", f.run)
	}
	if _, err := f.s.Update("w", func(r *Review) error { return Control(r, f.run.ID, ControlPause, "", 0, f.now()) }); err == nil {
		t.Fatal("paused an ended loop")
	}

	off := newLoop(t, LoopOff, 0)
	off.file("logged", "blocker")
	if a := off.roundEnded(); a.SendFindings != nil || off.run.Phase != PhaseDone || off.run.StopReason != "" {
		t.Fatalf("off is one pass: %+v %+v", a, off.run)
	}
	if _, err := LoopMode("sometimes"); err == nil || ClampRounds(0) != DefaultMaxRounds || ClampRounds(99) != 10 {
		t.Fatal("mode and rounds bounds")
	}
}

// A Runner restart reads the loop back where it was: round, phase, timeline,
// findings' accounting.
func TestLoopOutlivesARestart(t *testing.T) {
	f := newLoop(t, LoopAsk, 3)
	blocker := f.file("logged", "blocker")
	f.roundEnded()
	f.send(blocker.ID)
	f.authorTurn(2, 1, f.clock)
	again, err := New(f.dir)
	if err != nil {
		t.Fatal(err)
	}
	m, ok := again.ReviewerRun("e-rev")
	if !ok || m.Run.Round != 2 || m.Run.Phase != PhaseReviewing || len(m.Run.Rounds) != 2 || len(m.Run.Timeline) != len(f.run.Timeline) {
		t.Fatalf("after restart: %+v", m.Run)
	}
	rv, _ := again.Load("w")
	if a, err := RoundEnded(&rv, m.Run.ID, f.now()); err != nil || a.Escalate != "" {
		t.Fatal(a, err)
	}
	if rv.Comments[0].StillOpenRounds != 1 {
		t.Fatalf("still open: %+v", rv.Comments[0])
	}
}

func TestReviewerVerdictsOnlyOnItsOwnFindings(t *testing.T) {
	f := newLoop(t, LoopAsk, 3)
	mine := f.file("logged", "blocker")
	other, err := f.s.CreateRun("w", Run{ReviewerSession: "e-other", ReviewerLabel: "Claude", SourceSession: "e-auth"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.s.Update("w", func(r *Review) error { _, err := Verdict(r, other.ID, mine.ID, "resolved", "", f.now()); return err }); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("another review's verdict: %v", err)
	}
	if _, err = f.s.Update("w", func(r *Review) error { _, err := Verdict(r, f.run.ID, mine.ID, "fixed", "", f.now()); return err }); err == nil {
		t.Fatal("an unknown state was accepted")
	}
}

// Questions: one at a time per side, never both ways at once (each would wait
// on the other), answered by the turn that carried them.
func TestAsksBetweenTheAuthorAndTheReviewer(t *testing.T) {
	f := newLoop(t, LoopAsk, 3)
	var q Ask
	f.update(func(r *Review) (err error) {
		q, err = NewAsk(r, f.run.ID, Ask{From: RoleReviewer, To: RoleAuthor, FromSession: "e-rev", ToSession: "e-auth", FromLabel: "Codex", ToLabel: "Claude", Question: "Is Handle called from a goroutine?"}, f.now())
		return
	})
	if q.State != AskPending || !strings.HasPrefix(q.ID, "ask-") || q.Operation == "" {
		t.Fatalf("ask: %+v", q)
	}
	refuse := func(a Ask, want error, name string) {
		t.Helper()
		if _, err := f.s.Update("w", func(r *Review) error { _, err := NewAsk(r, f.run.ID, a, f.now()); return err }); !errors.Is(err, want) {
			t.Fatalf("%s: %v", name, err)
		}
	}
	refuse(Ask{From: RoleAuthor, To: RoleReviewer, FromLabel: "Claude", ToLabel: "Codex", Question: "x"}, ErrAskRefused, "a question back while one waits")
	refuse(Ask{From: RoleReviewer, To: RoleAuthor, FromLabel: "Codex", ToLabel: "Claude", Question: "again"}, ErrAskLimit, "a second question before the first is answered")
	var answered Ask
	var ok bool
	f.update(func(r *Review) error {
		answered, ok = AnswerAsk(r, f.run.ID, "e-auth", "other-op", "no", f.now())
		return nil
	})
	if ok {
		t.Fatal("another message's turn answered the question")
	}
	f.update(func(r *Review) error {
		answered, ok = AnswerAsk(r, f.run.ID, "e-auth", q.Operation, "Yes, from the worker pool.", f.now())
		return nil
	})
	if !ok || answered.State != AskAnswered || answered.Answer != "Yes, from the worker pool." {
		t.Fatal(answered, ok)
	}
	f.update(func(r *Review) error { _, ok = TellAsk(r, f.run.ID, q.ID); return nil })
	if !ok {
		t.Fatal("the answer was not the asker's")
	}
	if !strings.Contains(kinds(f.run), "asked answered") {
		t.Fatal(kinds(f.run))
	}
	// A turn begun before this Runner (no operation) answers the oldest question to that session.
	var second Ask
	f.update(func(r *Review) (err error) {
		second, err = NewAsk(r, f.run.ID, Ask{From: RoleAuthor, To: RoleReviewer, FromSession: "e-auth", ToSession: "e-rev", FromLabel: "Claude", ToLabel: "Codex", Question: "Which line?"}, f.now())
		return
	})
	f.update(func(r *Review) error { answered, ok = AnswerAsk(r, f.run.ID, "e-rev", "", "", f.now()); return nil })
	if !ok || answered.ID != second.ID || !strings.Contains(answered.Answer, "without a reply") {
		t.Fatal(answered, ok)
	}
	if _, err := CleanQuestion(" \n "); err == nil {
		t.Fatal("an empty question")
	}
}

// Findings the person sent while the reviewer was still at work: the round's
// end goes straight to waiting for the fix, counted from when they were sent.
func TestFindingsSentMidRoundWaitForTheFix(t *testing.T) {
	f := newLoop(t, LoopAsk, 3)
	blocker := f.file("logged", "blocker")
	f.update(func(r *Review) error {
		for i := range r.Comments {
			if r.Comments[i].ID == blocker.ID {
				r.Comments[i].Disposition, r.Comments[i].SentAt = Sent, f.now()
			}
		}
		FindingsSent(r, f.run.ID, []string{blocker.ID}, "You", "Claude", f.clock)
		return nil
	})
	if f.run.Phase != PhaseReviewing {
		t.Fatal("sending mid-round ended the round")
	}
	sent := f.clock
	f.roundEnded()
	if f.run.Phase != PhaseFixing || f.run.FixingSince != sent {
		t.Fatalf("after the round: %+v", f.run)
	}
	if a := f.authorTurn(2, 1, sent); !a.Rereview {
		t.Fatal("the fix begun right after sending did not start round 2")
	}
}
