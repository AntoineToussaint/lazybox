package work

import (
	"errors"
	"strings"
	"testing"
	"time"
)

func orchestrator(conversation string) Actor {
	return Actor{Kind: ActorAgent, Conversation: conversation}
}

func ceiling() Ceiling {
	return Ceiling{Permission: PermissionAsk, Profiles: []string{"claude-default", "codex-default"}, MaxChildren: 2, Depth: 1, BudgetUSD: 20}
}

func refused(t *testing.T, err error, want string) {
	t.Helper()
	if !errors.Is(err, ErrRefused) || !strings.Contains(err.Error(), want) {
		t.Fatalf("want a refusal saying %q, got %v", want, err)
	}
}

func child(conversation, permission, profile, grant string) AttemptPayload {
	return AttemptPayload{Conversation: conversation, Parent: "orch", Grant: grant, Permission: permission, Profile: profile, Role: "worker"}
}

// Only a person grants; a conversation holds one grant; a revoked grant
// starts nothing, and a restart reads the same grant back.
func TestOnlyAPersonGrantsAndARevokedGrantStartsNothing(t *testing.T) {
	dir := t.TempDir()
	s := newStore(t, dir)
	m := mustOK[*State](t)(s.Create(Human, "", "Ship search", ""))
	_, _, err := s.IssueGrant(m.ID, orchestrator("orch"), "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	refused(t, err, "only a person grants")
	_, _, err = s.IssueGrant(m.ID, Actor{Kind: ActorRule}, "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	refused(t, err, "only a person grants")
	_, grant, err := s.IssueGrant(m.ID, Human, "g1", GrantPayload{Holder: "orch", Title: "Orchestrator", Ceiling: ceiling()})
	if err != nil {
		t.Fatal(err)
	}
	// A retry is the same grant; a second one is refused.
	if _, again, err := s.IssueGrant(m.ID, Human, "g1", GrantPayload{Holder: "orch", Ceiling: ceiling()}); err != nil || again != grant {
		t.Fatalf("retry: %s %v", again, err)
	}
	_, _, err = s.IssueGrant(m.ID, Human, "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	refused(t, err, "already orchestrates")
	other := mustOK[*State](t)(s.Create(Human, "", "Other", ""))
	_, _, err = s.IssueGrant(other.ID, Human, "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	refused(t, err, "already orchestrates")
	if held, ok := s.HeldBy("orch"); !ok || held.Grant.Grant != grant || held.Grant.Ceiling.MaxChildren != 2 || len(held.Grant.Scopes) != len(GrantScopes) {
		t.Fatalf("held: %+v %v", held, ok)
	}
	// Bad ceilings are refused at the line that asked.
	for _, bad := range []Ceiling{{Permission: "inherit", Profiles: []string{"p"}, MaxChildren: 1, Depth: 1}, {Permission: PermissionAsk, MaxChildren: 1, Depth: 1}, {Permission: PermissionAsk, Profiles: []string{"p"}, MaxChildren: 99, Depth: 1}, {Permission: PermissionAsk, Profiles: []string{"p"}, MaxChildren: 1, Depth: 9}} {
		_, _, err = s.IssueGrant(other.ID, Human, "", GrantPayload{Holder: "x", Ceiling: bad})
		if !errors.Is(err, ErrRefused) {
			t.Fatalf("ceiling %+v accepted: %v", bad, err)
		}
	}
	// Spawning works while the grant is in force…
	if _, _, _, err = s.Spawn(m.ID, orchestrator("orch"), "s1", "", "Index", "", child("c1", PermissionAsk, "claude-default", grant)); err != nil {
		t.Fatal(err)
	}
	// …and a restart reads the grant and the lineage back.
	_ = s.Close()
	s = newStore(t, dir)
	lineage, ok := s.ParentOf("c1")
	if !ok || lineage.Attempt.Parent != "orch" || lineage.Attempt.Grant != grant || lineage.Attempt.Role != "worker" {
		t.Fatalf("lineage after restart: %+v %v", lineage, ok)
	}
	if _, err = s.RevokeGrant(m.ID, grant, orchestrator("orch"), "", "mine"); err == nil {
		t.Fatal("an orchestrator revoked its own root grant")
	}
	if _, err = s.RevokeGrant(m.ID, grant, Human, "", "enough"); err != nil {
		t.Fatal(err)
	}
	if _, ok := s.HeldBy("orch"); ok {
		t.Fatal("a revoked grant is still held")
	}
	_, _, _, err = s.Spawn(m.ID, orchestrator("orch"), "", "", "More", "", child("c2", PermissionAsk, "claude-default", grant))
	refused(t, err, "only an orchestrator with a grant")
	_, open, _ := s.AddTask(m.ID, Human, "", "", "Open task", "", nil)
	_, _, _, err = s.Spawn(m.ID, orchestrator("orch"), "", open, "", "", child("c2", PermissionAsk, "claude-default", grant))
	refused(t, err, "not in force")
}

// A child's permission, subscription and count stay within the ceiling, and
// an agent writes only what agentMay lists: never a gate, an acceptance, a
// cancellation, evidence or a brief.
func TestChildrenOnlyShrinkAndAgentsNeverDecide(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "Ship search", ""))
	_, grant, err := s.IssueGrant(m.ID, Human, "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	if err != nil {
		t.Fatal(err)
	}
	spawn := func(conversation, permission, profile string) error {
		_, _, _, err := s.Spawn(m.ID, orchestrator("orch"), "", "", "Task "+conversation, "", child(conversation, permission, profile, grant))
		return err
	}
	refused(t, spawn("c1", PermissionYolo, "claude-default"), "at ask at most")
	refused(t, spawn("c1", "inherit", "claude-default"), "at ask at most")
	refused(t, spawn("c1", PermissionAsk, "someone-elses"), "does not let children use")
	if err = spawn("c1", PermissionReadOnly, "codex-default"); err != nil {
		t.Fatal(err)
	}
	// Another agent cannot use the grant: only its holder starts children.
	_, _, _, err = s.Spawn(m.ID, orchestrator("intruder"), "", "", "Steal", "", AttemptPayload{Conversation: "c9", Parent: "orch", Grant: grant, Permission: PermissionAsk, Profile: "claude-default"})
	refused(t, err, "only an orchestrator with a grant")
	if err = spawn("c2", PermissionAsk, "claude-default"); err != nil {
		t.Fatal(err)
	}
	refused(t, spawn("c3", PermissionAsk, "claude-default"), "allows 2 agents at once")

	state := mustOK[*State](t)(s.Mission(m.ID))
	task := state.Children("orch")[0].Task
	attempt := state.Children("orch")[0].Attempt
	// A child reports its own task; nobody else's.
	_, err = s.Report(m.ID, task.ID, orchestrator("c2"), "", "not mine")
	refused(t, err, "only its own task")
	if _, err = s.Report(m.ID, task.ID, orchestrator("c1"), "", "indexed"); err != nil {
		t.Fatal(err)
	}
	// The orchestrator never decides a gate, cancels, edits or records evidence.
	_, err = s.Decide(m.ID, task.ID, orchestrator("orch"), "", true, "lgtm")
	refused(t, err, "only a person passes")
	_, err = s.CancelTask(m.ID, task.ID, orchestrator("orch"), "", "")
	refused(t, err, "cannot record task.canceled")
	_, err = s.EditTask(m.ID, task.ID, orchestrator("orch"), "", "x", "y")
	refused(t, err, "cannot record task.brief_changed")
	_, _, err = s.Append(m.ID, Draft{Subject: TaskSubject(task.ID), Type: EvidenceRecorded, Actor: orchestrator("orch"), Payload: Evidence{Kind: EvidenceRun, Ref: "run:x/y", Task: task.ID, Outcome: OutcomePass}})
	refused(t, err, "cannot record evidence.recorded")
	// A review verdict is the one evidence attributed to an agent (the reviewer).
	if _, _, err = s.Append(m.ID, Draft{Subject: TaskSubject(task.ID), Type: EvidenceRecorded, Actor: Actor{Kind: ActorAgent, Role: "reviewer", Conversation: "rev"}, Payload: Evidence{Kind: EvidenceReview, Ref: "review:r1", Task: task.ID, Outcome: OutcomePending}}); err != nil {
		t.Fatal(err)
	}
	_, err = s.EditMission(m.ID, orchestrator("orch"), "", "mine now", "")
	refused(t, err, "cannot record mission.brief_changed")
	// A person accepts.
	if _, err = s.Decide(m.ID, task.ID, Human, "", true, ""); err != nil {
		t.Fatal(err)
	}
	// Stopping: only the parent stops its child.
	_, err = s.StopChild(m.ID, task.ID, attempt.ID, orchestrator("c2"), "c1", "stopped")
	refused(t, err, "only the children it started")
	// A finished task frees its slot: a third child fits now.
	if err = spawn("c3", PermissionAsk, "claude-default"); err != nil {
		t.Fatal(err)
	}
}

// Messages go only between a parent and its child, at most 4 KB, at most six
// a minute each way, and only from the agent that sends them.
func TestMessagesFollowLineageSizeAndRate(t *testing.T) {
	s := newStore(t, t.TempDir())
	clock := time.Date(2026, 11, 16, 14, 0, 0, 0, time.UTC)
	s.Now = func() time.Time { return clock }
	m := mustOK[*State](t)(s.Create(Human, "", "Ship search", ""))
	_, grant, _ := s.IssueGrant(m.ID, Human, "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	if _, _, _, err := s.Spawn(m.ID, orchestrator("orch"), "", "", "Index", "", child("c1", PermissionAsk, "claude-default", grant)); err != nil {
		t.Fatal(err)
	}
	if _, _, _, err := s.Spawn(m.ID, orchestrator("orch"), "", "", "Rank", "", child("c2", PermissionAsk, "claude-default", grant)); err != nil {
		t.Fatal(err)
	}
	send := func(from, to, text string) error {
		_, _, err := s.Send(m.ID, orchestrator(from), "", MessagePayload{From: from, To: to, Text: text})
		return err
	}
	if err := send("orch", "c1", "use the new tokenizer"); err != nil {
		t.Fatal(err)
	}
	if err := send("c1", "orch", "done; see run r_12"); err != nil {
		t.Fatal(err)
	}
	refused(t, send("c1", "c2", "psst"), "only their parent or the children")
	refused(t, send("orch", "stranger", "hi"), "only their parent or the children")
	refused(t, send("orch", "c1", strings.Repeat("x", MaxMessage+1)), "1 to 4096 bytes")
	refused(t, send("orch", "c1", "  "), "1 to 4096 bytes")
	_, _, err := s.Send(m.ID, orchestrator("c1"), "", MessagePayload{From: "orch", To: "c1", Text: "spoofed"})
	refused(t, err, "sent by the sending agent")
	_, _, err = s.Send(m.ID, Human, "", MessagePayload{From: "orch", To: "c1", Text: "as a person"})
	refused(t, err, "sent by the sending agent")
	for i := 0; i < MessagesPerWindow-1; i++ {
		if err := send("orch", "c1", "again"); err != nil {
			t.Fatal(i, err)
		}
	}
	refused(t, send("orch", "c1", "one too many"), "at most 6 messages a minute")
	// The other direction has its own budget, and a minute later it refills.
	if err := send("c1", "orch", "ok"); err != nil {
		t.Fatal(err)
	}
	clock = clock.Add(MessageWindow + time.Second)
	if err := send("orch", "c1", "next minute"); err != nil {
		t.Fatal(err)
	}
	state := mustOK[*State](t)(s.Mission(m.ID))
	if len(state.Messages) != MessagesPerWindow+3 || state.Messages[1].From != "c1" || state.Messages[1].Kind != "note" {
		t.Fatalf("messages: %d %+v", len(state.Messages), state.Messages[1])
	}
}

// A grant narrowed for a child is a subset in every field and needs depth;
// revoking the root revokes it too.
func TestNarrowedGrantsAreSubsetsAndFallWithTheirRoot(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "Ship search", ""))
	wide := ceiling()
	wide.Depth = 2
	_, root, _ := s.IssueGrant(m.ID, Human, "", GrantPayload{Holder: "orch", Ceiling: wide})
	if _, _, _, err := s.Spawn(m.ID, orchestrator("orch"), "", "", "Lead", "", child("lead", PermissionAsk, "claude-default", root)); err != nil {
		t.Fatal(err)
	}
	narrow := func(c Ceiling, from, holder string) error {
		_, _, err := s.IssueGrant(m.ID, orchestrator(from), "", GrantPayload{Holder: holder, Parent: root, Ceiling: c})
		return err
	}
	for why, c := range map[string]Ceiling{
		"a wider permission": {Permission: PermissionYolo, Profiles: []string{"claude-default"}, MaxChildren: 1, Depth: 1, BudgetUSD: 5},
		"subscription":       {Permission: PermissionAsk, Profiles: []string{"other"}, MaxChildren: 1, Depth: 1, BudgetUSD: 5},
		"more children":      {Permission: PermissionAsk, Profiles: []string{"claude-default"}, MaxChildren: 3, Depth: 1, BudgetUSD: 5},
		"as much depth":      {Permission: PermissionAsk, Profiles: []string{"claude-default"}, MaxChildren: 1, Depth: 2, BudgetUSD: 5},
		"more budget":        {Permission: PermissionAsk, Profiles: []string{"claude-default"}, MaxChildren: 1, Depth: 1, BudgetUSD: 50},
	} {
		refused(t, narrow(c, "orch", "lead"), why)
	}
	sub := Ceiling{Permission: PermissionReadOnly, Profiles: []string{"claude-default"}, MaxChildren: 1, Depth: 1, BudgetUSD: 5}
	refused(t, narrow(sub, "lead", "lead"), "only the holder of a grant narrows it")
	refused(t, narrow(sub, "orch", "stranger"), "only for the holder's own child")
	if err := narrow(sub, "orch", "lead"); err != nil {
		t.Fatal(err)
	}
	if held, ok := s.HeldBy("lead"); !ok || held.Grant.Parent != root {
		t.Fatalf("lead's grant: %+v", held)
	}
	if _, err := s.RevokeGrant(m.ID, root, Human, "", ""); err != nil {
		t.Fatal(err)
	}
	if _, ok := s.HeldBy("lead"); ok {
		t.Fatal("a grant narrowed from a revoked one is still in force")
	}
}

func TestPermissionOrder(t *testing.T) {
	if NarrowerPermission(PermissionYolo, "inherit") != PermissionAsk || NarrowerPermission(PermissionReadOnly, PermissionYolo) != PermissionReadOnly || EffectivePermission("") != PermissionAsk {
		t.Fatal("permission order")
	}
}

// The store-wide check must run inside the append lock, even when two callers
// have already prepared valid grant drafts against different mission logs.
func TestGrantAppendEnforcesOneMissionPerHolder(t *testing.T) {
	s := newStore(t, t.TempDir())
	first := mustOK[*State](t)(s.Create(Human, "", "First mission", ""))
	second := mustOK[*State](t)(s.Create(Human, "", "Second mission", ""))
	issue := func(id string) error {
		g := GrantPayload{Holder: "orch", Grant: "grant-" + id, Ceiling: ceiling()}
		_, _, err := s.Append(id, Draft{Subject: GrantSubject(g.Grant), Type: GrantIssued, Actor: Human, Payload: g})
		return err
	}
	if err := issue(first.ID); err != nil {
		t.Fatal(err)
	}
	refused(t, issue(second.ID), "already orchestrates")
}
