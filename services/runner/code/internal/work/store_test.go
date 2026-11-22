package work

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func newStore(t *testing.T, dir string) *Store {
	t.Helper()
	s, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	clock := time.Date(2026, 11, 16, 14, 0, 0, 0, time.UTC)
	s.Now = func() time.Time { clock = clock.Add(time.Second); return clock }
	t.Cleanup(func() { _ = s.Close() })
	return s
}

func mustOK[T any](t *testing.T) func(T, error) T {
	return func(v T, err error) T {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
		return v
	}
}

func row(t *testing.T, s *Store, mission, task string, obs Observations) TaskView {
	t.Helper()
	state, err := s.Mission(mission)
	if err != nil {
		t.Fatal(err)
	}
	for _, v := range Derive(state, obs) {
		if v.ID == task {
			return v
		}
	}
	t.Fatalf("task %s not on the board", task)
	return TaskView{}
}

func live(conversation, state string) Observations {
	return Observations{Conversations: map[string]Liveness{conversation: {State: state}}}
}

func exit(code int) *int { return &code }

// The trust ladder end to end through the store: a report without evidence
// is claimed; a passing run makes it observed; CI on the same head verifies
// it; a person's acceptance makes it done.
func TestTrustLadderThroughTheStore(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "create", "Fix login", "The cookie expires early"))
	if m.Policy == nil || m.Policy.Preset() != "solo-pair" || m.Seq != 2 {
		t.Fatalf("a mission pins its process: %+v", m.Process)
	}
	_, task, err := s.AddTask(m.ID, Human, "task-1", "", "Repair the cookie", "Objective: …", nil)
	if err != nil {
		t.Fatal(err)
	}
	if v := row(t, s, m.ID, task, Observations{}); v.Status != StatusReady || v.Trust != "" {
		t.Fatalf("new task: %+v", v)
	}
	if _, _, err = s.Assign(m.ID, task, Human, "assign-1", AttemptPayload{Conversation: "c1", Workspace: "ws"}); err != nil {
		t.Fatal(err)
	}
	if v := row(t, s, m.ID, task, live("c1", LiveRunning)); v.Status != StatusWorking || v.Trust != TrustObserved {
		t.Fatalf("a running attempt is working, observed: %+v", v)
	}
	if v := row(t, s, m.ID, task, live("c1", LiveWaiting)); v.Status != StatusNeedsYou || v.Gate != "" {
		t.Fatalf("an agent asking is needs-you without a gate: %+v", v)
	}
	if _, err = s.Report(m.ID, task, Human, "report-1", "Done"); err != nil {
		t.Fatal(err)
	}
	v := row(t, s, m.ID, task, live("c1", LiveIdle))
	if v.Status != StatusNeedsYou || v.Gate != "reviewed" || v.Trust != TrustClaimed || !v.Awaiting() {
		t.Fatalf("a report without evidence stays claimed and asks for acceptance: %+v", v)
	}
	// Evidence keeps arriving while the gate is open.
	if n, err := s.Record(Origin{Conversation: "c1"}, Actor{Kind: ActorLazybox}, Evidence{Kind: EvidenceRun, Ref: "run:e1/r1", Title: "go test ./...", Outcome: OutcomeFail, ExitCode: exit(1), Revisions: map[string]string{"api": "aaa"}}); err != nil || n != 1 {
		t.Fatalf("run evidence not attached: %d %v", n, err)
	}
	if v = row(t, s, m.ID, task, live("c1", LiveIdle)); v.Trust != TrustClaimed || !contains(v.Flags, FlagFailing) {
		t.Fatalf("a failing run is flagged: %+v", v)
	}
	s.Record(Origin{Conversation: "c1"}, Actor{Kind: ActorLazybox}, Evidence{Kind: EvidenceRun, Ref: "run:e1/r2", Title: "go test ./...", Outcome: OutcomePass, ExitCode: exit(0), Revisions: map[string]string{"api": "bbb"}})
	if v = row(t, s, m.ID, task, live("c1", LiveIdle)); v.Trust != TrustObserved || contains(v.Flags, FlagFailing) {
		t.Fatalf("a passing run is observed: %+v", v)
	}
	// Checks only attach once the conversation links that pull request.
	pr := Evidence{Kind: EvidencePR, Ref: "pr:team/api#4@aaa", Member: "api", Repository: "team/api", Number: 4, Head: "aaa", Outcome: OutcomePass}
	if n, err := s.Record(Origin{Workspace: "ws"}, Actor{Kind: ActorCheck}, pr); n != 0 || err != nil {
		t.Fatalf("workspace-only checks attached: %d %v", n, err)
	}
	link := Evidence{Kind: EvidencePR, Ref: PullRequestLinkRef("team/api", 4), Repository: "team/api", Number: 4, Outcome: OutcomeUnknown}
	if n, err := s.Record(Origin{Conversation: "c1"}, Actor{Kind: ActorLazybox}, link); n != 1 || err != nil {
		t.Fatalf("delivery link: %d %v", n, err)
	}
	// Checks on an older head do not verify it.
	s.Record(Origin{Workspace: "ws"}, Actor{Kind: ActorCheck}, Evidence{Kind: EvidencePR, Ref: "pr:team/api#4", Member: "api", Repository: "team/api", Number: 4, Head: "aaa", Outcome: OutcomePass})
	if v = row(t, s, m.ID, task, live("c1", LiveIdle)); v.Trust != TrustObserved || !contains(v.Flags, FlagStale) {
		t.Fatalf("checks on an older head are stale: %+v", v)
	}
	s.Record(Origin{Workspace: "ws"}, Actor{Kind: ActorCheck}, Evidence{Kind: EvidencePR, Ref: "pr:team/api#4", Member: "api", Repository: "team/api", Number: 4, Head: "bbb", Outcome: OutcomePass})
	if v = row(t, s, m.ID, task, live("c1", LiveIdle)); v.Trust != TrustVerified || contains(v.Flags, FlagStale) {
		t.Fatalf("checks on the newest head verify: %+v", v)
	}
	// The same observation is recorded once.
	before := mustOK[*State](t)(s.Mission(m.ID)).Seq
	if n, _ := s.Record(Origin{Workspace: "ws"}, Actor{Kind: ActorCheck}, Evidence{Kind: EvidencePR, Ref: "pr:team/api#4", Member: "api", Repository: "team/api", Number: 4, Head: "bbb", Outcome: OutcomePass}); n != 0 || mustOK[*State](t)(s.Mission(m.ID)).Seq != before {
		t.Fatal("an unchanged pull request was recorded again")
	}
	if _, err = s.Decide(m.ID, task, Human, "accept-1", true, "Ship it"); err != nil {
		t.Fatal(err)
	}
	if v = row(t, s, m.ID, task, live("c1", LiveIdle)); v.Status != StatusDone || v.Trust != TrustAccepted || v.EvidenceTrust != TrustVerified {
		t.Fatalf("accepted: %+v", v)
	}
	// A done task takes no late report or evidence, from anyone.
	if _, err = s.Report(m.ID, task, Actor{Kind: ActorAgent, Conversation: "c1"}, "late", "again"); !errors.Is(err, ErrRefused) {
		t.Fatal("late report accepted", err)
	}
	if n, _ := s.Record(Origin{Conversation: "c1"}, Actor{Kind: ActorLazybox}, Evidence{Kind: EvidenceRun, Ref: "run:e1/r3", Outcome: OutcomeFail, ExitCode: exit(2)}); n != 0 {
		t.Fatal("evidence attached to a done task")
	}
}

func contains(list []string, value string) bool {
	for _, v := range list {
		if v == value {
			return true
		}
	}
	return false
}

func TestGatesArePassedOnlyByAPerson(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "M", ""))
	_, task, _ := s.AddTask(m.ID, Human, "", "", "T", "", nil)
	s.Assign(m.ID, task, Human, "", AttemptPayload{Conversation: "c1"})
	if _, err := s.Decide(m.ID, task, Human, "", true, ""); !errors.Is(err, ErrRefused) {
		t.Fatal("a gate passed before it was requested", err)
	}
	s.Report(m.ID, task, Human, "", "done")
	for _, actor := range []Actor{{Kind: ActorAgent, Conversation: "c1"}, {Kind: ActorRule}, {Kind: ActorCheck}, {Kind: ActorLazybox}} {
		if _, err := s.Decide(m.ID, task, actor, "", true, ""); !errors.Is(err, ErrRefused) || !strings.Contains(err.Error(), "only a person") {
			t.Fatalf("%s passed a human gate: %v", actor.Kind, err)
		}
	}
	if _, err := s.Report(m.ID, task, Human, "", "again"); !errors.Is(err, ErrRefused) {
		t.Fatal("a second report while waiting for a decision", err)
	}
	if _, err := s.Decide(m.ID, task, Human, "", false, "Add a test"); err != nil {
		t.Fatal(err)
	}
	if v := row(t, s, m.ID, task, live("c1", LiveRunning)); v.Status != StatusChangesRequested || !strings.Contains(v.Reason, "Add a test") {
		t.Fatalf("rejected: %+v", v)
	}
	if _, err := s.Report(m.ID, task, Human, "", "with a test"); err != nil {
		t.Fatal(err)
	}
	if v := row(t, s, m.ID, task, live("c1", LiveIdle)); v.Status != StatusNeedsYou || v.Gate == "" {
		t.Fatalf("a new report asks again: %+v", v)
	}
}

func TestAssignmentHandsOffAndDeletionFails(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "M", ""))
	_, task, _ := s.AddTask(m.ID, Human, "", "", "T", "", nil)
	_, first, err := s.Assign(m.ID, task, Human, "a1", AttemptPayload{Conversation: "c1", Workspace: "ws"})
	if err != nil {
		t.Fatal(err)
	}
	// Retried with the same key: the same attempt, nothing written.
	seq := mustOK[*State](t)(s.Mission(m.ID)).Seq
	if _, again, _ := s.Assign(m.ID, task, Human, "a1", AttemptPayload{Conversation: "c1", Workspace: "ws"}); again != first || mustOK[*State](t)(s.Mission(m.ID)).Seq != seq {
		t.Fatal("a retried assignment wrote again")
	}
	_, second, err := s.Assign(m.ID, task, Human, "a2", AttemptPayload{Conversation: "c2", Workspace: "ws"})
	if err != nil || second == first {
		t.Fatal(err)
	}
	state := mustOK[*State](t)(s.Mission(m.ID))
	if attempts := state.Task(task).Attempts; len(attempts) != 2 || !attempts[0].Ended || attempts[0].Outcome != "handoff" || attempts[1].Ended {
		t.Fatalf("hand-off: %+v", attempts)
	}
	// Evidence from the earlier conversation no longer reaches the task.
	if n, _ := s.Record(Origin{Conversation: "c1"}, Actor{Kind: ActorLazybox}, Evidence{Kind: EvidenceRun, Ref: "run:x", Outcome: OutcomePass, ExitCode: exit(0)}); n != 0 {
		t.Fatal("a handed-off conversation still attaches evidence")
	}
	if err := s.EndAttempts("c2", "deleted"); err != nil {
		t.Fatal(err)
	}
	if v := row(t, s, m.ID, task, Observations{}); v.Status != StatusFailed || v.Trust != TrustObserved {
		t.Fatalf("an attempt deleted before a report fails the task: %+v", v)
	}
	if len(s.Attempts()) != 0 {
		t.Fatal("an ended attempt is still live")
	}
}

func TestBlockedAndCanceled(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "M", ""))
	_, first, _ := s.AddTask(m.ID, Human, "", "", "First", "", nil)
	_, second, err := s.AddTask(m.ID, Human, "", "", "Second", "", []string{first})
	if err != nil {
		t.Fatal(err)
	}
	if v := row(t, s, m.ID, second, Observations{}); v.Status != StatusBlocked || !strings.Contains(v.Reason, "First") {
		t.Fatalf("blocked: %+v", v)
	}
	if _, _, err := s.Append(m.ID, Draft{Subject: TaskSubject(first), Type: TaskEdgeAdded, Actor: Human, Payload: EdgePayload{Task: first, Kind: "blocks", After: second}}); !errors.Is(err, ErrRefused) {
		t.Fatal("a cycle was accepted", err)
	}
	if _, _, err := s.AddTask(m.ID, Human, "", "", "Orphan", "", []string{"t_missing"}); !errors.Is(err, ErrRefused) {
		t.Fatal("a missing predecessor was accepted", err)
	}
	s.CancelTask(m.ID, first, Human, "", "not needed")
	if v := row(t, s, m.ID, first, Observations{}); v.Status != StatusCanceled {
		t.Fatalf("canceled: %+v", v)
	}
	s.CancelMission(m.ID, Human, "", "")
	if v := row(t, s, m.ID, second, Observations{}); v.Status != StatusCanceled {
		t.Fatalf("a canceled mission cancels its tasks: %+v", v)
	}
	if _, _, err := s.AddTask(m.ID, Human, "", "", "Late", "", nil); !errors.Is(err, ErrRefused) {
		t.Fatal("a canceled mission took a task", err)
	}
}

// A restarted Runner reads every mission back, continues each log's
// sequence, and derives the same board.
func TestRestartResumesTheLog(t *testing.T) {
	dir := t.TempDir()
	s := newStore(t, dir)
	m := mustOK[*State](t)(s.Create(Human, "", "Persisted", "brief"))
	_, task, _ := s.AddTask(m.ID, Human, "", "", "T", "", nil)
	s.Assign(m.ID, task, Human, "", AttemptPayload{Conversation: "c1", Workspace: "ws"})
	s.Record(Origin{Conversation: "c1"}, Actor{Kind: ActorLazybox}, Evidence{Kind: EvidenceRun, Ref: "run:e/r", Title: "make test", Outcome: OutcomePass, ExitCode: exit(0)})
	s.Report(m.ID, task, Human, "", "done")
	before := mustOK[*State](t)(s.Mission(m.ID))
	want := board(before, live("c1", LiveIdle))
	if err := s.Close(); err != nil {
		t.Fatal(err)
	}
	reopened := newStore(t, dir)
	after := mustOK[*State](t)(reopened.Mission(m.ID))
	if after.Seq != before.Seq || len(after.Events) != len(before.Events) {
		t.Fatalf("seq %d → %d", before.Seq, after.Seq)
	}
	if got := board(after, live("c1", LiveIdle)); string(mustJSON(t, got)) != string(mustJSON(t, want)) {
		t.Fatalf("board changed across a restart:\n%s\n%s", mustJSON(t, want), mustJSON(t, got))
	}
	if attempts := reopened.Attempts(); len(attempts) != 1 || attempts[0].Attempt.Conversation != "c1" {
		t.Fatalf("live attempts after restart: %+v", attempts)
	}
	if _, err := reopened.Decide(m.ID, task, Human, "", true, ""); err != nil {
		t.Fatal(err)
	}
	if got := mustOK[*State](t)(reopened.Mission(m.ID)); got.Seq != before.Seq+1 || got.Events[len(got.Events)-1].Seq != before.Seq+1 {
		t.Fatal("the sequence did not continue")
	}
}

// A Runner stopped mid-write leaves a line without its newline: it was never
// acknowledged, so it is dropped, and the mission stays writable.
func TestTornTailIsTruncated(t *testing.T) {
	dir := t.TempDir()
	s := newStore(t, dir)
	m := mustOK[*State](t)(s.Create(Human, "", "Torn", ""))
	s.Close()
	path := filepath.Join(dir, "missions", m.ID+".jsonl")
	f, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	f.WriteString(`{"id":"x","seq":3,"type":"task.cre`)
	f.Close()
	reopened := newStore(t, dir)
	state, err := reopened.Mission(m.ID)
	if err != nil || state.Seq != 2 {
		t.Fatalf("torn tail: %v seq %d", err, state.Seq)
	}
	if _, _, err := reopened.AddTask(m.ID, Human, "", "", "after", "", nil); err != nil {
		t.Fatal(err)
	}
	if got := readLog(t, path); len(got) != 3 || got[2].Seq != 3 {
		t.Fatalf("log after repair: %d events", len(got))
	}
}

// One damaged mission is listed with its error; the others load.
func TestDamagedMissionIsIsolated(t *testing.T) {
	dir := t.TempDir()
	s := newStore(t, dir)
	good := mustOK[*State](t)(s.Create(Human, "", "Good", ""))
	s.Close()
	if err := os.WriteFile(filepath.Join(dir, "missions", "m_bad.jsonl"), []byte("{not json}\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	reopened := newStore(t, dir)
	list := reopened.Missions()
	if len(list) != 2 {
		t.Fatalf("missions: %d", len(list))
	}
	var bad, ok bool
	for _, m := range list {
		bad = bad || (m.State.ID == "m_bad" && m.Err != nil)
		ok = ok || (m.State.ID == good.ID && m.Err == nil)
	}
	if !bad || !ok {
		t.Fatalf("listing: %+v", list)
	}
	if _, _, err := reopened.AddTask("m_bad", Human, "", "", "x", "", nil); err == nil {
		t.Fatal("a damaged mission was written")
	}
}

func TestRefusalsSayWhy(t *testing.T) {
	s := newStore(t, t.TempDir())
	if _, err := s.Create(Human, "", "  ", ""); !errors.Is(err, ErrRefused) {
		t.Fatal("untitled mission", err)
	}
	if _, err := s.Create(Human, "", "M", strings.Repeat("x", maxBrief+1)); !errors.Is(err, ErrRefused) {
		t.Fatal("oversized brief", err)
	}
	m := mustOK[*State](t)(s.Create(Human, "same", "M", ""))
	if again := mustOK[*State](t)(s.Create(Human, "same", "M", "")); again.ID != m.ID || len(s.Missions()) != 1 {
		t.Fatal("a retried create made a second mission")
	}
	if _, _, err := s.Append(m.ID, Draft{Subject: "task:x", Type: "task.exploded", Actor: Human}); !errors.Is(err, ErrRefused) {
		t.Fatal("an unknown type was written", err)
	}
	if _, _, err := s.Append(m.ID, Draft{Subject: "task:x", Type: TaskReported, Actor: Human, Payload: ReportPayload{Task: "x"}}); !errors.Is(err, ErrRefused) {
		t.Fatal("a report on a missing task", err)
	}
	if _, _, err := s.Append("m_missing", Draft{Type: TaskCreated}); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("missing mission", err)
	}
	if _, _, err := s.Assign(m.ID, "t_missing", Human, "", AttemptPayload{}); !errors.Is(err, ErrRefused) {
		t.Fatal("an assignment without a conversation", err)
	}
	if got := mustOK[*State](t)(s.Mission(m.ID)); got.Seq != 2 {
		t.Fatalf("refused events were written: seq %d", got.Seq)
	}
}

func TestIDsSortByTime(t *testing.T) {
	a := newID(time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC))
	b := newID(time.Date(2026, 1, 1, 0, 0, 0, 1e6, time.UTC))
	if len(a) != 26 || len(b) != 26 || !(a < b) {
		t.Fatalf("ids %s %s", a, b)
	}
}

// The process comes from internal/process: solo pair asks a person to accept
// a report at its "reviewed" gate. A mission pins the gates, so its log keeps
// meaning the same after a release changes the presets; a preset whose
// reports need a verifier is refused until a slice runs one.
func TestPolicyComesFromTheProcessAndIsPinned(t *testing.T) {
	p, err := PolicyFor("")
	if err != nil || p.Preset() != "solo-pair" || p.ReportGate() != "reviewed" || p.DoneGate() != "reviewed" || p.Autonomy() != 3 || !strings.HasPrefix(p.Digest(), "sha256:") {
		t.Fatalf("solo pair: %+v %v", p, err)
	}
	if _, err := PolicyFor("reviewed-team"); !errors.Is(err, ErrRefused) {
		t.Fatal("a preset whose reports go to a verifier was accepted", err)
	}
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "Pinned", ""))
	if m.Process.ReportGate != "reviewed" || m.Process.DoneGate != "reviewed" || m.Process.Digest != p.Digest() {
		t.Fatalf("pinned: %+v", m.Process)
	}
	// A pinned payload is read as pinned, even for a preset this Runner no longer has.
	old, err := policyOf(ProcessPayload{Preset: "retired", ReportGate: "accept", DoneGate: "accept"})
	if err != nil || old.ReportGate() != "accept" {
		t.Fatalf("pinned gates ignored: %v", err)
	}
}

func TestPullRequestEvidenceFollowsLinkedAttempt(t *testing.T) {
	s := newStore(t, t.TempDir())
	assign := func(title, conversation string) (string, string) {
		m := mustOK[*State](t)(s.Create(Human, "", title, ""))
		_, task, err := s.AddTask(m.ID, Human, "", "", title, "", nil)
		if err != nil {
			t.Fatal(err)
		}
		if _, _, err := s.Assign(m.ID, task, Human, "", AttemptPayload{Conversation: conversation, Workspace: "shared"}); err != nil {
			t.Fatal(err)
		}
		return m.ID, task
	}
	one, task := assign("One", "worker")
	two, other := assign("Two", "neighbor")
	link := Evidence{Kind: EvidencePR, Ref: PullRequestLinkRef("team/api", 12), Repository: "team/api", Number: 12, Outcome: OutcomeUnknown}
	pr := Evidence{Kind: EvidencePR, Ref: "pr:team/api#12@abc", Repository: "team/api", Member: "api", Number: 12, Head: "abc", Outcome: OutcomePass}
	record := func(origin Origin, e Evidence, want int) {
		t.Helper()
		if n, err := s.Record(origin, Actor{Kind: ActorLazybox}, e); n != want || err != nil {
			t.Fatalf("%v %s recorded %d, want %d: %v", origin, e.Ref, n, want, err)
		}
	}
	record(Origin{Workspace: "shared"}, pr, 0)
	record(Origin{Conversation: "worker"}, link, 1)
	record(Origin{Workspace: "shared"}, pr, 1)
	if len(mustOK[*State](t)(s.Mission(two)).Task(other).Evidence) != 0 {
		t.Fatal("another mission in the same workspace received checks")
	}
	if _, _, err := s.Assign(one, task, Human, "", AttemptPayload{Conversation: "replacement", Workspace: "shared"}); err != nil {
		t.Fatal(err)
	}
	if trust, flags, _ := judge(mustOK[*State](t)(s.Mission(one)).Task(task)); trust != TrustClaimed || !contains(flags, FlagNoEvidence) {
		t.Fatalf("replacement inherited earlier checks: %s %v", trust, flags)
	}
	pr.Outcome = OutcomeFail
	record(Origin{Workspace: "shared"}, pr, 0)
	assign("Three", "replacement")
	record(Origin{Conversation: "replacement"}, link, 0)
	if len(mustOK[*State](t)(s.Mission(one)).Task(task).Evidence) != 2 {
		t.Fatal("historical evidence changed after reassignment")
	}
}

func TestRefreshAfterLinkingLegacyPullRequest(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "Upgrade", ""))
	_, task, err := s.AddTask(m.ID, Human, "", "", "Delivery", "", nil)
	if err != nil {
		t.Fatal(err)
	}
	_, attempt, err := s.Assign(m.ID, task, Human, "", AttemptPayload{Workspace: "ws", Conversation: "worker"})
	if err != nil {
		t.Fatal(err)
	}
	pr := Evidence{Kind: EvidencePR, Repository: "team/api", Member: "api", Number: 12, Ref: "pr:team/api#12@abc", Head: "abc", Outcome: OutcomePass, Task: task, Attempt: attempt}
	if _, _, err := s.Append(m.ID, Draft{Subject: TaskSubject(task), Type: EvidenceRecorded, Actor: Actor{Kind: ActorCheck}, Payload: pr}); err != nil {
		t.Fatal(err)
	}
	record := func(origin Origin, ev Evidence) {
		t.Helper()
		if n, err := s.Record(origin, Actor{Kind: ActorLazybox}, ev); n != 1 || err != nil {
			t.Fatalf("record %s: %d %v", ev.Ref, n, err)
		}
	}
	record(Origin{Conversation: "worker"}, Evidence{Kind: EvidencePR, Repository: pr.Repository, Number: pr.Number, Ref: PullRequestLinkRef(pr.Repository, pr.Number)})
	record(Origin{Workspace: "ws"}, pr)
	next := pr
	next.Ref, next.Head = "pr:team/api#12@def", "def"
	record(Origin{Workspace: "ws"}, next)
	record(Origin{Workspace: "ws"}, pr)
	if trust, flags, reason := judge(mustOK[*State](t)(s.Mission(m.ID)).Task(task)); trust != TrustVerified || len(flags) != 0 {
		t.Fatalf("%s %v: %s", trust, flags, reason)
	}
}
