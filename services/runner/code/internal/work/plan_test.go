package work

import (
	"testing"
)

func plan(tasks ...PlanTask) PlanPayload {
	return PlanPayload{Summary: "split search in three", Tasks: tasks}
}

// An orchestrator proposes; only a person approves, with a cap that becomes
// the grant's; approval creates the tasks in order; a new proposal replaces
// one still waiting; a rejected plan creates nothing.
func TestPlansAreProposedByTheOrchestratorAndDecidedByThePerson(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "Ship search", ""))
	c := ceiling()
	c.PlanRequired = true
	_, grant, err := s.IssueGrant(m.ID, Human, "grant-op", GrantPayload{Holder: "orch", Ceiling: c})
	if err != nil {
		t.Fatal(err)
	}
	index := PlanTask{Key: "index", Title: "Index the corpus", Paths: []string{"search/"}, EstimateUSD: 4, Role: "worker"}
	rank := PlanTask{Key: "rank", Title: "Rank results", Paths: []string{"search/rank.go"}, EstimateUSD: 3, After: []string{"index"}, Permission: PermissionReadOnly}
	docs := PlanTask{Key: "docs", Title: "Document it", Paths: []string{"docs/"}, EstimateUSD: 1}

	_, _, err = s.ProposePlan(m.ID, orchestrator("orch"), "grant-op", PlanPayload{Grant: grant, Tasks: []PlanTask{index}})
	refused(t, err, "operation key already used")
	// Who may propose, and what a plan may hold.
	_, _, err = s.ProposePlan(m.ID, orchestrator("stranger"), "", PlanPayload{Grant: grant, Tasks: []PlanTask{index}})
	refused(t, err, "only an orchestrator with a grant")
	for why, bad := range map[string][]PlanTask{
		"allows ask at most": {{Key: "a", Title: "x", Permission: PermissionYolo}},
		"does not allow":     {{Key: "a", Title: "x", Profile: "other"}},
		"not another task":   {{Key: "a", Title: "x", After: []string{"b"}}},
		"both \"a\"":         {{Key: "a", Title: "x"}, {Key: "a", Title: "y"}},
		"has a cycle":        {{Key: "a", Title: "x", After: []string{"b"}}, {Key: "b", Title: "y", After: []string{"a"}}},
		"paths are relative": {{Key: "a", Title: "x", Paths: []string{"/etc/passwd"}}},
		"needs a title":      {{Key: "a"}},
	} {
		p := PlanPayload{Grant: grant, Tasks: bad}
		_, _, err = s.ProposePlan(m.ID, orchestrator("orch"), "", p)
		refused(t, err, why)
	}
	p := plan(index, rank)
	p.Grant = grant
	_, first, err := s.ProposePlan(m.ID, orchestrator("orch"), "", p)
	if err != nil {
		t.Fatal(err)
	}
	p = plan(index, rank, docs)
	p.Grant = grant
	state, second, err := s.ProposePlan(m.ID, orchestrator("orch"), "", p)
	if err != nil {
		t.Fatal(err)
	}
	if state.PlanOf(first).State != PlanStateSuperseded || state.PendingPlan(grant).Plan != second || state.PendingPlan(grant).TotalUSD() != 8 {
		t.Fatalf("plans: %+v", state.Plans)
	}
	if overlaps := state.PlanOf(second).Overlaps(); len(overlaps) != 1 || overlaps[0].Path != "search/rank.go" || len(overlaps[0].Keys) != 2 {
		t.Fatalf("overlaps: %+v", overlaps)
	}
	// The orchestrator never decides.
	_, err = s.DecidePlan(m.ID, second, orchestrator("orch"), "", true, 20, 0, "")
	refused(t, err, "an agent cannot record plan.approved")
	_, err = s.DecidePlan(m.ID, first, Human, "", true, 20, 0, "")
	refused(t, err, "not waiting for a decision")
	state, err = s.DecidePlan(m.ID, second, Human, "approve", true, 12, 2, "go")
	if err != nil {
		t.Fatal(err)
	}
	approved := state.PlanOf(second)
	if approved.State != PlanStateApproved || len(approved.Created) != 3 || state.Grant(grant).Ceiling.BudgetUSD != 12 || state.Grant(grant).Ceiling.MaxChildren != 2 {
		t.Fatalf("approved: %+v grant %+v", approved, state.Grant(grant).Ceiling)
	}
	rankTask := state.Task(approved.Created["rank"])
	if rankTask == nil || len(rankTask.BlockedBy) != 1 || rankTask.BlockedBy[0] != approved.Created["index"] {
		t.Fatalf("order: %+v", rankTask)
	}
	if entry, ok := state.ApprovedTask(grant, approved.Created["rank"]); !ok || entry.Permission != PermissionReadOnly || entry.EstimateUSD != 3 {
		t.Fatalf("approved task: %+v %v", entry, ok)
	}
	// A retried approval changes nothing.
	if again, err := s.DecidePlan(m.ID, second, Human, "approve", true, 12, 2, "go"); err != nil || len(again.Tasks) != 3 {
		t.Fatalf("retry: %v %d", err, len(again.Tasks))
	}
	// A rejected plan creates nothing.
	p = plan(docs)
	p.Grant = grant
	_, third, _ := s.ProposePlan(m.ID, orchestrator("orch"), "", p)
	state, err = s.DecidePlan(m.ID, third, Human, "", false, 0, 0, "not now")
	if err != nil || state.PlanOf(third).State != PlanStateRejected || len(state.Tasks) != 3 {
		t.Fatalf("reject: %v", err)
	}
	// A narrowed grant cannot drop the plan requirement.
	if ok, why := (Ceiling{Permission: PermissionAsk, Profiles: []string{"claude-default"}, MaxChildren: 1, Depth: 0, BudgetUSD: 5}).Within(Ceiling{Permission: PermissionAsk, Profiles: []string{"claude-default"}, MaxChildren: 1, Depth: 1, BudgetUSD: 10, PlanRequired: true}); ok || why != "the right to start agents without an approved plan" {
		t.Fatalf("narrowing: %v %s", ok, why)
	}
}

// A budget stop is recorded once per crossing; only a person raises the cap,
// which clears it.
func TestBudgetStopsAreRecordedOnceAndRaisedByThePerson(t *testing.T) {
	s := newStore(t, t.TempDir())
	m := mustOK[*State](t)(s.Create(Human, "", "Ship search", ""))
	_, grant, _ := s.IssueGrant(m.ID, Human, "", GrantPayload{Holder: "orch", Ceiling: ceiling()})
	if _, wrote, err := s.RecordBudget(m.ID, grant, BudgetExhausted, BudgetPayload{CapUSD: 20, SpentUSD: 19, ReservedUSD: 4}); err != nil || !wrote {
		t.Fatal(err, wrote)
	}
	if _, wrote, err := s.RecordBudget(m.ID, grant, BudgetExhausted, BudgetPayload{CapUSD: 20, SpentUSD: 19.5}); err != nil || wrote {
		t.Fatal("recorded twice", err)
	}
	state, _ := s.Mission(m.ID)
	if state.BudgetState(grant) != "exhausted" {
		t.Fatal(state.BudgetState(grant))
	}
	_, err := s.RaiseBudget(m.ID, grant, orchestrator("orch"), "", 100)
	refused(t, err, "an agent cannot record budget.raised")
	_, _, err = s.Append(m.ID, Draft{Subject: GrantSubject(grant), Type: BudgetExhausted, Actor: orchestrator("orch"), Payload: BudgetPayload{Grant: grant}})
	refused(t, err, "cannot record budget.exhausted")
	state, err = s.RaiseBudget(m.ID, grant, Human, "", 40)
	if err != nil || state.BudgetState(grant) != "" || state.Grant(grant).Ceiling.BudgetUSD != 40 {
		t.Fatalf("raise: %v %q", err, state.BudgetState(grant))
	}
}
