package runner

import (
	"context"
	"path/filepath"
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
	if note := finish(b, generator, 1); note != "prerequisite generate: blocked on "+candidate+" (spec "+spec.ID+" v1)" {
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
	next.reconcileDelivery()
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
	b.reconcileDelivery()
	b.reconcileDelivery()
	if got := b.Delivery.Observations(); len(got) != 1 || got[0].Classify() != delivery.Passed {
		t.Fatalf("retry recorded %+v", got)
	}
}
