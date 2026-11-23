package runner

import (
	"errors"
	"fmt"
	"path/filepath"
	"slices"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Delivery evidence (audits/2026-11-22/delivery-outcomes-roadmap.md, milestone C).
//
// A `lazybox run --check C` (or `--needs C`) resolves its criterion when it
// registers: against the approved contract of the conversation's Mission
// attempt, or else the spec revision the conversation bound with `lazybox
// delivery bind`. The check is saved on the command. When the run finishes,
// the Runner reads each workspace repository's HEAD as the candidate and saves
// the observation with the command's completion, then records it in the
// delivery journal. A Runner that stops in between replays saved observations
// when it starts and every reconcileEvery; the journal keeps one per run.

const reconcileEvery = time.Minute

func (b *Bench) openDelivery() error {
	store, err := delivery.OpenStore(filepath.Join(b.Config.State, "delivery.jsonl"))
	if err != nil {
		return err
	}
	b.Delivery = store
	b.reconcileDelivery()
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		ticker := time.NewTicker(reconcileEvery)
		defer ticker.Stop()
		for {
			select {
			case <-b.ctx.Done():
				return
			case <-ticker.C:
				b.reconcileDelivery()
			}
		}
	}()
	return nil
}

// missionContract is the approved contract of the conversation's live Mission
// attempt: found reports an attempt, even one whose plan has no contract.
func (b *Bench) missionContract(conversation string) (check delivery.Check, contract delivery.Contract, found bool, err error) {
	if b.Work == nil || conversation == "" {
		return
	}
	for _, at := range b.Work.Attempts() {
		if at.Attempt.Conversation != conversation {
			continue
		}
		state, err := b.Work.Mission(at.Mission)
		if err != nil {
			return check, contract, true, err
		}
		check = delivery.Check{Attempt: at.Attempt.ID, Subject: delivery.Subject{Conversation: conversation, Mission: at.Mission, Task: at.Task}}
		if _, plan := state.ApprovedTaskPlan(at.Attempt.Grant, at.Task); plan != nil && plan.Contract != nil {
			contract = fromPlanContract(plan.Contract)
		}
		return check, contract, true, nil
	}
	return
}

func fromPlanContract(p *work.PlanContract) delivery.Contract {
	c := delivery.Contract{Artifact: p.Artifact, Version: p.Version, Digest: p.Digest, Schema: "spec.v1", Legacy: p.Criteria}
	if len(p.Bindings) > 0 {
		c.Schema, c.Criteria, c.Legacy = "spec.v2", p.Bindings, nil
	}
	return c
}

// resolveCheck is what a run's --check or --needs counts for, refused before
// the command runs when nothing names the criterion.
func (b *Bench) resolveCheck(e *Execution, criterion string, prerequisite bool) (*delivery.Check, error) {
	if b.Delivery == nil {
		return nil, errors.New("delivery evidence is unavailable on this Runner")
	}
	conversation := conversationOf(e)
	check, contract, mission, err := b.missionContract(conversation)
	switch {
	case err != nil:
		return nil, err
	case mission && !contract.Bound():
		return nil, errors.New("this Mission task's approved plan pins no spec.v2 contract with named criteria; ask the person to approve a plan pinned to one")
	case !mission:
		binding, ok := b.Delivery.Binding(conversation)
		if !ok {
			return nil, errors.New("no spec is bound to this conversation: publish a spec.v2 (`lazybox artifact template delivery-spec`), then `lazybox delivery bind <spec-id>@<version>`")
		}
		contract = binding.Contract
		check = delivery.Check{Attempt: binding.Attempt, Subject: delivery.Subject{Conversation: conversation}}
	}
	ids := make([]string, 0, len(contract.Criteria))
	for _, c := range contract.Criteria {
		ids = append(ids, c.ID)
	}
	if !slices.Contains(ids, criterion) {
		return nil, fmt.Errorf("spec %s v%d has no criterion %q (it has %v)", contract.Artifact, contract.Version, criterion, ids)
	}
	check.CriterionID, check.Prerequisite = criterion, prerequisite
	check.ContractDigest, check.Artifact, check.Version = contract.Digest, contract.Artifact, contract.Version
	return &check, nil
}

// BindDelivery pins a published spec.v2 revision to e's conversation. A
// conversation working a Mission task is judged against the approved plan.
func (b *Bench) BindDelivery(e *Execution, artifactID string, version uint32) (delivery.Binding, error) {
	if b.Delivery == nil || b.Artifacts == nil {
		return delivery.Binding{}, errors.New("delivery evidence is unavailable on this Runner")
	}
	conversation := conversationOf(e)
	if _, _, mission, err := b.missionContract(conversation); err != nil {
		return delivery.Binding{}, err
	} else if mission {
		return delivery.Binding{}, errors.New("this conversation works a Mission task: its checks count against the plan's approved contract")
	}
	if artifactID == "" || version == 0 {
		return delivery.Binding{}, errors.New("give a spec artifact ID and a positive version")
	}
	doc, err := b.Artifacts.Get(e.CollectionID, artifactID, version)
	if err != nil {
		return delivery.Binding{}, err
	}
	if doc.Kind != "spec" || doc.Format != "json" {
		return delivery.Binding{}, errors.New("bind a structured spec (JSON, kind spec)")
	}
	contract, err := delivery.ContractFromSpec(artifactID, version, artifacts.Digest(doc.Content), doc.Content)
	if err != nil {
		return delivery.Binding{}, err
	}
	if !contract.Bound() {
		return delivery.Binding{}, errors.New("spec.v1 criteria have no IDs to check: publish a spec.v2 version (`lazybox artifact template delivery-spec`) and bind that")
	}
	return b.Delivery.Bind(conversation, contract)
}

// observeCheck adds to report what a finished check run observed, on the
// revisions its workspace holds now.
func (b *Bench) observeCheck(state commands.Status, report *commands.Report, signal string) {
	check := state.Spec.Check
	e, err := b.Get(state.Spec.SessionID)
	if err != nil {
		report.DeliveryError = "the conversation is gone: " + err.Error()
		return
	}
	revisions := b.revisions(e.CollectionID)
	if len(revisions) == 0 {
		report.DeliveryError = "the workspace has no repository revisions to say what was checked"
		return
	}
	var exit *int
	if !report.Piped {
		code := report.ExitCode
		exit = &code
	}
	ref := "run:" + e.ID + "/" + state.ID
	o := check.Observe(ref, ref, delivery.CandidateID(revisions), exit, signal != "")
	report.Observation = &o
}

// recordObservation records a finished run's saved observation. A failure
// leaves it saved with the run for reconcileDelivery.
func (b *Bench) recordObservation(state commands.Status) error {
	if b.Delivery == nil || state.Report == nil || state.Report.Observation == nil {
		return nil
	}
	_, err := b.Delivery.Record(*state.Report.Observation)
	return err
}

// reconcileDelivery records every saved observation the journal lacks.
func (b *Bench) reconcileDelivery() {
	if b.Delivery == nil || b.Commands == nil {
		return
	}
	for _, state := range b.Commands.List("") {
		if state.Done {
			_ = b.recordObservation(state)
		}
	}
}

// DeliveryNote is what a finished check run tells its caller: what it
// recorded, or why it recorded nothing.
func (b *Bench) DeliveryNote(state commands.Status) string {
	check := state.Spec.Check
	if check == nil || !state.Done || state.Report == nil {
		return ""
	}
	if state.Report.DeliveryError != "" {
		return "delivery evidence not recorded: " + state.Report.DeliveryError
	}
	o := state.Report.Observation
	if o == nil {
		return ""
	}
	if err := b.recordObservation(state); errors.Is(err, delivery.ErrConflict) {
		return o.Summary(*check) + "; not recorded: " + err.Error()
	} else if err != nil {
		return o.Summary(*check) + "; not yet in the delivery journal (" + err.Error() + "), the Runner retries"
	}
	return o.Summary(*check)
}
