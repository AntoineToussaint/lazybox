package runner

import (
	"context"
	"errors"
	"fmt"
	"log"
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
// delivery bind`. It also fingerprints every workspace repository then (its
// commit and any uncommitted changes, codecheckpoints.Fingerprint): that is
// the candidate. The check is saved on the command. When the run finishes the
// Runner fingerprints again; a run whose files changed meanwhile records
// nothing. The observation is recorded in the delivery journal and saved with
// the command's completion. A Runner that stopped before recording it replays
// saved observations when it starts; a failed write is retried every
// reconcileEvery. The journal keeps one observation per run.

const reconcileEvery = time.Minute

func (b *Bench) openDelivery() error {
	store, err := delivery.OpenStore(filepath.Join(b.Config.State, "delivery.jsonl"))
	if err != nil {
		return err
	}
	b.Delivery = store
	if n := store.Skipped(); n > 0 {
		log.Printf("delivery journal: %d unreadable record(s) skipped", n)
	}
	b.deliveryPending = map[string]error{}
	// After a restart, any finished run may hold an unrecorded observation.
	for _, state := range b.Commands.List("") {
		if state.Done {
			b.recordObservation(state)
		}
	}
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
				b.retryDelivery()
			}
		}
	}()
	return nil
}

// missionContract is the approved contract of the conversation's live Mission
// attempt: found reports an attempt, even one whose plan has no contract. A
// conversation working several tasks at once cannot say which one a check is
// for, so it is refused, as deliveryAttempt treats it.
func (b *Bench) missionContract(conversation string) (check delivery.Check, contract delivery.Contract, found bool, err error) {
	if b.Work == nil || conversation == "" {
		return
	}
	var attempts []work.Attachment
	for _, at := range b.Work.Attempts() {
		if at.Attempt.Conversation == conversation {
			attempts = append(attempts, at)
		}
	}
	if len(attempts) > 1 {
		return check, contract, true, fmt.Errorf("this conversation works %d Mission tasks at once, so a check cannot tell which one it counts for", len(attempts))
	}
	for _, at := range attempts {
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
	candidate, err := b.candidate(e.CollectionID)
	if err != nil {
		return nil, fmt.Errorf("cannot say which files the check runs on: %w", err)
	}
	check.CriterionID, check.Prerequisite, check.Candidate = criterion, prerequisite, candidate
	check.ContractDigest, check.Artifact, check.Version = contract.Digest, contract.Artifact, contract.Version
	return &check, nil
}

// candidate fingerprints every repository of the workspace. A repository that
// is not ready or cannot be read refuses it: a candidate never names part of
// the workspace.
func (b *Bench) candidate(workspace string) (string, error) {
	if b.Collections == nil || b.Checkpoints == nil {
		return "", errors.New("workspace fingerprints are unavailable on this Runner")
	}
	w, err := b.Collections.Get(workspace)
	if err != nil {
		return "", err
	}
	if len(w.Members) == 0 {
		return "", errors.New("the workspace has no repository")
	}
	ctx, cancel := context.WithTimeout(b.ctx, 30*time.Second)
	defer cancel()
	revisions := map[string]string{}
	for _, m := range w.Members {
		if !m.Ready {
			return "", fmt.Errorf("repository %s is not ready", m.Name)
		}
		revision, err := b.Checkpoints.Fingerprint(ctx, m.Path)
		if err != nil {
			return "", fmt.Errorf("repository %s: %w", m.Name, err)
		}
		revisions[m.Name] = revision
	}
	return delivery.CandidateID(revisions), nil
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
// files fingerprinted when it registered; a run whose files changed before it
// finished observed something no candidate names, and records nothing.
func (b *Bench) observeCheck(state commands.Status, report *commands.Report, signal string) {
	check := state.Spec.Check
	e, err := b.Get(state.Spec.SessionID)
	if err != nil {
		report.DeliveryError = "the conversation is gone: " + err.Error()
		return
	}
	now, err := b.candidate(e.CollectionID)
	switch {
	case err != nil:
		report.DeliveryError = "cannot say which files the check ran on: " + err.Error()
		return
	case now != check.Candidate:
		report.DeliveryError = "the workspace's files changed while the check ran (a commit or an edit); run it again"
		return
	}
	var exit *int
	if !report.Piped {
		code := report.ExitCode
		exit = &code
	}
	ref := "run:" + e.ID + "/" + state.ID
	o := check.Observe(ref, ref, check.Candidate, exit, signal != "")
	report.Observation = &o
}

// recordObservation records a run's observation in the delivery journal. A
// failed write is kept pending for retryDelivery and DeliveryNote.
func (b *Bench) recordObservation(state commands.Status) {
	if b.Delivery == nil || state.Report == nil || state.Report.Observation == nil {
		return
	}
	_, err := b.Delivery.Record(*state.Report.Observation)
	b.deliveryMu.Lock()
	defer b.deliveryMu.Unlock()
	if err != nil && !errors.Is(err, delivery.ErrConflict) {
		b.deliveryPending[state.ID] = err
	} else {
		delete(b.deliveryPending, state.ID)
	}
}

// retryDelivery records the observations a failed write left pending.
func (b *Bench) retryDelivery() {
	b.deliveryMu.Lock()
	ids := make([]string, 0, len(b.deliveryPending))
	for id := range b.deliveryPending {
		ids = append(ids, id)
	}
	b.deliveryMu.Unlock()
	for _, id := range ids {
		if c, err := b.Commands.Get(id); err == nil {
			b.recordObservation(c.Status())
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
	if o == nil || b.Delivery == nil {
		return ""
	}
	if b.Delivery.Has(o.ID) {
		return o.Summary(*check)
	}
	b.deliveryMu.Lock()
	err := b.deliveryPending[state.ID]
	b.deliveryMu.Unlock()
	if err == nil {
		return o.Summary(*check) + "; not recorded: a different observation is already recorded for this run"
	}
	return o.Summary(*check) + "; not yet in the delivery journal (" + err.Error() + "), the Runner retries"
}
