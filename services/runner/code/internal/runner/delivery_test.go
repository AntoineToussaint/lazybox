package runner

import (
	"context"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

const catalogSpec = `{"schema":"spec.v2","problem":"Deliver the work catalog","requirements":["Persist work items"],"acceptanceCriteria":[` +
	`{"id":"storage","expected":"Duplicate inserts keep one item","required":true},` +
	`{"id":"generate","expected":"The catalog service is generated","required":true},` +
	`{"id":"api","expected":"An authorized client reads the item","required":true}]}`

// The catalog sequence in an ordinary conversation: checks are refused until
// a spec is bound, a failed prerequisite blocks, an asserted check passes, and
// a completion the Runner saved but never recorded is replayed exactly once
// after a restart. A new commit is a new candidate.
func TestCheckRunsRecordDeliveryEvidenceAcrossRestart(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	spec, err := b.Artifacts.Put(ws.ID, artifacts.Put{Title: "Catalog", Content: catalogSpec})
	if err != nil || spec.Kind != "spec" {
		t.Fatal(spec.Kind, err)
	}

	if _, err := b.RegisterPipe(e, "go test", false, &PipeCheck{Criterion: "storage"}); err == nil || !strings.Contains(err.Error(), "lazybox delivery bind") {
		t.Fatalf("checked with no bound spec: %v", err)
	}
	binding, err := b.BindDelivery(e, spec.ID, 1)
	if err != nil || binding.Attempt != "conversation:"+conversationOf(e)+"/1" {
		t.Fatal(binding, err)
	}
	if _, err := b.RegisterPipe(e, "go test", false, &PipeCheck{Criterion: "docs"}); err == nil || !strings.Contains(err.Error(), "storage") {
		t.Fatalf("checked an unknown criterion: %v", err)
	}

	member := ws.Members[0].Path
	candidate := delivery.CandidateID(map[string]string{"api": gitIn(t, member, "rev-parse", "HEAD")})
	finish := func(b *Bench, c *commands.Command, exit int) string {
		t.Helper()
		if err := b.FinishPipe(c, nil, "", &commands.Report{ExitCode: exit}); err != nil {
			t.Fatal(err)
		}
		return b.DeliveryNote(c.Status())
	}
	generator, err := b.RegisterPipe(e, "codefly add service catalog", false, &PipeCheck{Criterion: "generate", Prerequisite: true})
	if err != nil {
		t.Fatal(err)
	}
	if note := finish(b, generator, 1); note != "prerequisite generate: blocked on "+candidate+" (“Catalog” v1)" {
		t.Fatal(note)
	}
	storage, _ := b.RegisterPipe(e, "psql -f storage.sql", false, &PipeCheck{Criterion: "storage"})
	if note := finish(b, storage, 0); !strings.HasPrefix(note, "check storage: passed on "+candidate) {
		t.Fatal(note)
	}

	// A commit makes a new candidate: the repaired generator counts for it alone.
	gitIn(t, member, "commit", "--allow-empty", "-m", "generate the catalog")
	repaired := delivery.CandidateID(map[string]string{"api": gitIn(t, member, "rev-parse", "HEAD")})
	rerun, _ := b.RegisterPipe(e, "codefly add service catalog", false, &PipeCheck{Criterion: "generate"})
	if note := finish(b, rerun, 0); !strings.HasPrefix(note, "check generate: passed on "+repaired) || repaired == candidate {
		t.Fatal(note)
	}

	// The Runner saves the API check's completion, then stops before recording it.
	api, _ := b.RegisterPipe(e, "go test ./api", false, &PipeCheck{Criterion: "api"})
	report := &commands.Report{ExitCode: 0}
	b.observeCheck(api.Status(), report, "")
	if err := api.FinishPipeSignalled(nil, "", report); err != nil {
		t.Fatal(err)
	}
	if len(b.Delivery.Observations()) != 3 {
		t.Fatal("recorded before the simulated stop")
	}
	if err := b.Detach(); err != nil {
		t.Fatal(err)
	}
	next, err := New(b.Config)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	if got := len(next.Delivery.Observations()); got != 4 {
		t.Fatalf("starting replayed %d observations, want 4", got)
	}
	// A late retry of the same finish and later sweeps add nothing.
	next.retryDelivery()
	restored, err := next.Commands.Get(api.Status().ID)
	if err != nil {
		t.Fatal(err)
	}
	if note := finish(next, restored, 0); !strings.HasPrefix(note, "check api: passed on "+repaired) {
		t.Fatal(note)
	}
	observations := next.Delivery.Observations()
	if len(observations) != 4 {
		t.Fatalf("%d observations after restart, want 4: %+v", len(observations), observations)
	}
	first := delivery.Scope{ContractDigest: binding.Contract.Digest, Candidate: candidate, Attempt: binding.Attempt}
	if a := delivery.Assess(first, binding.Contract, observations); a.Verdict != delivery.Blocked || a.Criteria["storage"] != delivery.Passed || a.Criteria["api"] != delivery.Unrun {
		t.Fatal("the first candidate", a)
	}
	second := first
	second.Candidate = repaired
	if a := delivery.Assess(second, binding.Contract, observations); a.Verdict != delivery.Incomplete || a.Criteria["generate"] != delivery.Passed || a.Criteria["api"] != delivery.Passed || a.Criteria["storage"] != delivery.Unrun {
		t.Fatal("the repaired candidate", a)
	}
}

// A conversation working a Mission task checks against the plan's approved
// contract: it cannot bind its own spec, and a plan with no spec.v2 contract
// refuses checks before they run.
func TestMissionChecksUseTheApprovedContract(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	spec, err := b.Artifacts.Put(ws.ID, artifacts.Put{Title: "Catalog", Content: catalogSpec})
	if err != nil {
		t.Fatal(err)
	}
	m, err := b.Work.Create(work.Human, "", "Catalog", "")
	if err != nil {
		t.Fatal(err)
	}
	_, task, err := b.Work.AddTask(m.ID, work.Human, "", "", "API", "", nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := b.Work.Assign(m.ID, task, work.Human, "", work.AttemptPayload{Conversation: conversationOf(e), Workspace: e.CollectionID}); err != nil {
		t.Fatal(err)
	}
	if _, err := b.BindDelivery(e, spec.ID, 1); err == nil || !strings.Contains(err.Error(), "Mission task") {
		t.Fatalf("bound a spec over the Mission's contract: %v", err)
	}
	if _, err := b.RegisterPipe(e, "go test", false, &PipeCheck{Criterion: "api"}); err == nil || !strings.Contains(err.Error(), "approved plan") {
		t.Fatalf("checked without an approved contract: %v", err)
	}
}

// A journal that cannot be written says so after the run and the sweep records
// the saved observation once it can.
func TestUnrecordedObservationIsVisibleAndRetried(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	spec, err := b.Artifacts.Put(ws.ID, artifacts.Put{Title: "Catalog", Content: catalogSpec})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := b.BindDelivery(e, spec.ID, 1); err != nil {
		t.Fatal(err)
	}
	run, err := b.RegisterPipe(e, "go test ./api", false, &PipeCheck{Criterion: "api"})
	if err != nil {
		t.Fatal(err)
	}
	healthy := b.Delivery
	path := filepath.Join(b.Config.State, "delivery.jsonl")
	broken, err := delivery.OpenStore(path)
	if err != nil {
		t.Fatal(err)
	}
	_ = broken.Close() // every write now fails
	b.Delivery = broken
	if err := b.FinishPipe(run, nil, "", &commands.Report{ExitCode: 0}); err != nil {
		t.Fatal(err)
	}
	if note := b.DeliveryNote(run.Status()); !strings.HasPrefix(note, "check api: passed on candidate:") || !strings.Contains(note, "the Runner retries") {
		t.Fatalf("the failure was not visible: %q", note)
	}
	b.Delivery = healthy
	b.retryDelivery()
	b.retryDelivery()
	if got := b.Delivery.Observations(); len(got) != 1 || got[0].Classify() != delivery.Passed {
		t.Fatalf("retry recorded %+v", got)
	}
}

// The candidate is the files the check runs on: uncommitted changes make
// their own, and a run whose files change before it finishes records nothing.
func TestCheckCandidateCoversUncommittedChanges(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	spec, err := b.Artifacts.Put(ws.ID, artifacts.Put{Title: "Catalog", Content: catalogSpec})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := b.BindDelivery(e, spec.ID, 1); err != nil {
		t.Fatal(err)
	}
	member := ws.Members[0].Path
	clean := delivery.CandidateID(map[string]string{"api": gitIn(t, member, "rev-parse", "HEAD")})
	if err := os.WriteFile(filepath.Join(member, "fix.go"), []byte("package api\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	dirty, err := b.RegisterPipe(e, "go test ./api", false, &PipeCheck{Criterion: "api"})
	if err != nil {
		t.Fatal(err)
	}
	if dirty.Status().Spec.Check.Candidate == clean {
		t.Fatal("uncommitted changes kept the committed candidate")
	}
	if err := b.FinishPipe(dirty, nil, "", &commands.Report{ExitCode: 0}); err != nil {
		t.Fatal(err)
	}
	if note := b.DeliveryNote(dirty.Status()); !strings.HasPrefix(note, "check api: passed on "+dirty.Status().Spec.Check.Candidate) {
		t.Fatal(note)
	}

	// An edit while the check runs: what it tested is no candidate.
	moving, err := b.RegisterPipe(e, "go test ./api", false, &PipeCheck{Criterion: "api"})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(member, "fix.go"), []byte("package api\n\nconst edited = true\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := b.FinishPipe(moving, nil, "", &commands.Report{ExitCode: 0}); err != nil {
		t.Fatal(err)
	}
	if note := b.DeliveryNote(moving.Status()); !strings.Contains(note, "not recorded") || !strings.Contains(note, "changed while the check ran") {
		t.Fatal(note)
	}
	if got := len(b.Delivery.Observations()); got != 1 {
		t.Fatalf("%d observations, want only the dirty-tree pass", got)
	}
}

// A conversation working two Mission tasks cannot say which one a check is for.
func TestCheckRefusesAmbiguousMissionAttempts(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	m, err := b.Work.Create(work.Human, "", "Catalog", "")
	if err != nil {
		t.Fatal(err)
	}
	for _, title := range []string{"API", "Storage"} {
		_, task, err := b.Work.AddTask(m.ID, work.Human, "", "", title, "", nil)
		if err != nil {
			t.Fatal(err)
		}
		if _, _, err := b.Work.Assign(m.ID, task, work.Human, "", work.AttemptPayload{Conversation: conversationOf(e), Workspace: e.CollectionID}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := b.RegisterPipe(e, "go test", false, &PipeCheck{Criterion: "api"}); err == nil || !strings.Contains(err.Error(), "2 Mission tasks") {
		t.Fatalf("an ambiguous check was accepted: %v", err)
	}
}

// Release qualification with a Mission (roadmap milestones E and G): a task
// worked under an approved spec.v2 plan has one outcome whether it is looked
// up by its conversation or by the task, before and after a Runner restart;
// the person's acceptance is reported beside a verdict that keeps its gap.
func TestMissionOutcomeIsTheSameByConversationAndTask(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	parent, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Permission: "ask"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	spec, err := b.Artifacts.Put(ws.ID, artifacts.Put{Title: "Catalog", Content: catalogSpec})
	if err != nil {
		t.Fatal(err)
	}
	contract, err := delivery.ContractFromSpec(spec.ID, 1, artifacts.Digest(catalogSpec), catalogSpec)
	if err != nil {
		t.Fatal(err)
	}
	m, err := b.Work.Create(work.Human, "", "Catalog", "")
	if err != nil {
		t.Fatal(err)
	}
	_, grant, err := b.Work.IssueGrant(m.ID, work.Human, "", work.GrantPayload{Holder: parent.ID, Ceiling: work.Ceiling{Permission: "ask", Profiles: []string{"codex-default"}, MaxChildren: 1, Depth: 1}})
	if err != nil {
		t.Fatal(err)
	}
	actor := work.Actor{Kind: work.ActorAgent, Role: "orchestrator", Conversation: parent.ID, Execution: parent.ID}
	_, plan, err := b.Work.ProposePlan(m.ID, actor, "", work.PlanPayload{Grant: grant, Tasks: []work.PlanTask{{Key: "api", Title: "API"}},
		Contract: &work.PlanContract{Artifact: spec.ID, Version: 1, Workspace: ws.ID, Digest: contract.Digest, Criteria: contract.Lines(), Bindings: contract.Criteria}})
	if err != nil {
		t.Fatal(err)
	}
	state, err := b.Work.DecidePlan(m.ID, plan, work.Human, "", true, 0, 1, "")
	if err != nil {
		t.Fatal(err)
	}
	task := state.PlanOf(plan).Created["api"]
	if _, _, err := b.Work.Assign(m.ID, task, actor, "", work.AttemptPayload{Conversation: conversationOf(e), Workspace: ws.ID, Grant: grant, Parent: parent.ID, Role: "worker", Profile: "codex-default", Permission: "ask"}); err != nil {
		t.Fatal(err)
	}
	finish := func(criterion string, needs bool, exit int) {
		t.Helper()
		c, err := b.RegisterPipe(e, criterion, false, &PipeCheck{Criterion: criterion, Prerequisite: needs})
		if err != nil {
			t.Fatal(err)
		}
		if err := b.FinishPipe(c, nil, "", &commands.Report{ExitCode: exit}); err != nil {
			t.Fatal(err)
		}
	}
	finish("generate", true, 1)
	finish("storage", false, 0)
	same := func(b *Bench, e *Execution) delivery.Outcome {
		t.Helper()
		byConversation, err := b.DeliveryOutcome(e)
		if err != nil {
			t.Fatal(err)
		}
		byTask, err := b.TaskDeliveryOutcome(m.ID, task)
		if err != nil || !reflect.DeepEqual(byConversation, byTask) {
			t.Fatalf("the lookups disagree:\n%+v\n%+v\n%v", byConversation, byTask, err)
		}
		return byTask
	}
	o := same(b, e)
	if o.Subject.Mission != m.ID || o.Subject.Task != task || o.Candidates[0].Verdict != delivery.Blocked || !o.Candidates[0].Current {
		t.Fatalf("Mission outcome: %+v", o)
	}
	if blocking := o.Candidates[0].Blocking(); len(blocking) != 2 || blocking[0].ID != "generate" || blocking[1].ID != "api" {
		t.Fatalf("blocking: %+v", blocking)
	}

	// The person accepts with the generator still blocked: the gap stays visible.
	if _, err := b.Work.Report(m.ID, task, work.Actor{Kind: work.ActorAgent, Conversation: conversationOf(e)}, "", "Storage works; generation is blocked"); err != nil {
		t.Fatal(err)
	}
	if _, err := b.Work.Decide(m.ID, task, work.Human, "", true, "ship storage first"); err != nil {
		t.Fatal(err)
	}
	accepted := same(b, e)
	if accepted.Decision != delivery.Accepted || accepted.Candidates[0].Verdict != delivery.Blocked {
		t.Fatalf("acceptance changed the verdict: %+v", accepted)
	}

	if err := b.Detach(); err != nil {
		t.Fatal(err)
	}
	next, err := New(b.Config)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	ne, err := next.Get(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if after := same(next, ne); !reflect.DeepEqual(after, accepted) {
		t.Fatalf("restart changed the outcome:\n%+v\n%+v", accepted, after)
	}
}

// Found running the Runner live: right after a commit, the outcome must name
// the new files as current, never repeat a verdict on the files it replaced.
func TestOutcomeReadsTheCurrentFilesOnEveryLookup(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	spec, err := b.Artifacts.Put(ws.ID, artifacts.Put{Title: "Catalog", Content: catalogSpec})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := b.BindDelivery(e, spec.ID, 1); err != nil {
		t.Fatal(err)
	}
	for _, criterion := range []string{"storage", "generate", "api"} {
		c, err := b.RegisterPipe(e, criterion, false, &PipeCheck{Criterion: criterion})
		if err != nil {
			t.Fatal(err)
		}
		if err := b.FinishPipe(c, nil, "", &commands.Report{ExitCode: 0}); err != nil {
			t.Fatal(err)
		}
	}
	before, err := b.DeliveryOutcome(e)
	if err != nil || before.Candidates[0].Verdict != delivery.Passed || !before.Candidates[0].Current {
		t.Fatalf("before the commit: %+v %v", before, err)
	}
	gitIn(t, ws.Members[0].Path, "commit", "--allow-empty", "-m", "after the checks")
	after, err := b.DeliveryOutcome(e)
	if err != nil {
		t.Fatal(err)
	}
	if head := after.Candidates[0]; !head.Current || head.Candidate == before.Current || head.Verdict != delivery.Incomplete {
		t.Fatalf("a commit later, the old files still read as current: %+v", after.Candidates)
	}
	if older := after.Candidates[1]; older.Candidate != before.Current || older.Current || older.Verdict != delivery.Passed {
		t.Fatalf("the checked files are not kept as earlier: %+v", after.Candidates)
	}
}
