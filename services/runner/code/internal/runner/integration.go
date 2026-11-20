package runner

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"slices"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

type reviewedWorker struct{ heads map[string]string }

func (b *Bench) integrationInputs(ctx context.Context, s *work.State, plan string) (*work.Plan, []reviewedWorker, map[string]string, error) {
	p := s.PlanOf(plan)
	if s.Canceled || p == nil || p.State != work.PlanStateApproved || p.Contract == nil || len(p.Baseline) == 0 {
		return nil, nil, nil, fmt.Errorf("%w: an active mission with an approved pinned contract is required", work.ErrRefused)
	}
	base, err := b.planBaseline(ctx, p.Contract.Workspace)
	if err != nil || !slices.Equal(base, p.Baseline) {
		return nil, nil, nil, fmt.Errorf("%w: approved base changed or is unreadable", work.ErrRefused)
	}
	refs := map[string]string{}
	var workers []reviewedWorker
	for _, entry := range p.Tasks {
		task := s.Task(p.Created[entry.Key])
		if entry.Workspace != "pinned" || task == nil || task.Canceled || task.Current() == nil || task.Current().Grant != p.Grant {
			return nil, nil, nil, fmt.Errorf("%w: task %s must have a pinned worker", work.ErrRefused, entry.Key)
		}
		at := task.Current()
		review := s.IntegrationReview(task)
		if review == nil || review.Outcome != work.OutcomePass || review.ContractDigest != p.Contract.Digest || review.Workspace != at.Workspace || len(review.Revisions) != len(base) {
			return nil, nil, nil, fmt.Errorf("%w: task %s requires a passing candidate review", work.ErrRefused, entry.Key)
		}
		members, err := b.planBaseline(ctx, at.Workspace)
		if err != nil || len(members) != len(base) {
			return nil, nil, nil, fmt.Errorf("%w: task %s workspace changed", work.ErrRefused, entry.Key)
		}
		for i, m := range members {
			if m.Name != base[i].Name || m.Repository != base[i].Repository || review.Revisions[m.Name] != m.Commit {
				return nil, nil, nil, fmt.Errorf("%w: stale review for %s/%s", work.ErrRefused, entry.Key, m.Name)
			}
		}
		refs[task.ID] = review.ReviewRevision()
		workers = append(workers, reviewedWorker{review.Revisions})
	}
	return p, workers, refs, nil
}

func (b *Bench) PrepareCandidate(ctx context.Context, mission, key, plan string, argv []string) (*work.State, string, error) {
	if key == "" || len(argv) == 0 || argv[0] == "" {
		return nil, "", fmt.Errorf("%w: supply an operation key and combined check", work.ErrRefused)
	}
	unlock := b.orch().lock("integration/" + mission)
	defer unlock()
	s, err := b.Work.Mission(mission)
	if err != nil {
		return nil, "", err
	}
	if e, ok := s.Event(key); ok {
		c := s.Candidate(e.Subject)
		if e.Type != work.CandidateReserved || c == nil || c.Plan != plan || !slices.Equal(c.Check, argv) {
			return nil, "", fmt.Errorf("%w: operation key already used", work.ErrRefused)
		}
		if c.State == "preparing" || c.State == "checking" {
			recovered := *c
			cause := errors.New("Runner interrupted candidate preparation; use a fresh request key")
			if c.State == "checking" {
				for _, status := range b.Commands.ListWorkspace(c.ID) {
					if status.Spec.Kind != "integration" || !slices.Equal(status.Spec.Argv, c.Check) {
						continue
					}
					cmd, err := b.Commands.Get(status.ID)
					if err != nil {
						return nil, "", err
					}
					cause = b.observeCandidate(b.ctx, &recovered, cmd)
					break
				}
			}
			return b.finishCandidate(mission, recovered, cause)
		}
		return s, c.ID, nil
	}
	p, workers, refs, err := b.integrationInputs(ctx, s, plan)
	if err != nil {
		return nil, "", err
	}
	base := p.Baseline
	if prev := s.Candidate(s.AcceptedCandidate); prev != nil {
		if len(prev.Members) != len(base) {
			return nil, "", errors.New("accepted manifest membership changed")
		}
		for i, m := range base {
			if m.Name != prev.Members[i].Name || m.Repository != prev.Members[i].Repository {
				return nil, "", errors.New("accepted manifest membership changed")
			}
		}
		base = prev.Members
	}
	c := work.Candidate{ID: commands.ID(), Plan: plan, Generation: s.Generation, State: "preparing", Reviews: refs, Check: argv}
	_, reserved, err := b.Work.Append(mission, work.Draft{Type: work.CandidateReserved, Subject: c.ID, Idempotency: key, Actor: work.Human, Payload: c})
	if err != nil {
		return nil, "", err
	}
	if len(reserved) == 0 {
		return nil, "", fmt.Errorf("%w: reservation key already used", work.ErrRefused)
	}
	// A dropped client cannot interrupt the command between recording its inputs and its result.
	err = b.checkCandidate(b.ctx, mission, &c, base, p, workers)
	return b.finishCandidate(mission, c, err)
}

func (b *Bench) finishCandidate(mission string, c work.Candidate, err error) (*work.State, string, error) {
	if b.ctx.Err() != nil {
		return nil, c.ID, b.ctx.Err()
	}
	c.State = "passed"
	drafts := []work.Draft{{Type: work.CandidateChecked, Subject: c.ID, Actor: work.Actor{Kind: work.ActorLazybox}}}
	if err != nil {
		c.State, c.Detail = "failed", truncateRunes(err.Error(), 1000)
		task := commands.ID()
		c.RepairTask = task
		drafts = append(drafts, work.Draft{Type: work.TaskCreated, Subject: work.TaskSubject(task), Actor: work.Actor{Kind: work.ActorLazybox}, Payload: work.TaskPayload{Task: task, Title: "Repair integration candidate " + c.ID, Brief: c.Detail + "\nInspect candidate workspace " + c.ID + " and combined check " + c.Command + ". Repair the worker inputs and review the new revisions."}})
	}
	drafts[0].Payload = c
	s, _, err := b.Work.Append(mission, drafts...)
	return s, c.ID, err
}
func (b *Bench) checkCandidate(ctx context.Context, mission string, c *work.Candidate, base []work.BaselineMember, p *work.Plan, workers []reviewedWorker) error {
	spec := collections.Spec{Name: "Integration " + c.ID}
	for _, m := range base {
		spec.Members = append(spec.Members, collections.MemberSpec{Name: m.Name, RepositoryID: m.Repository, BaseRef: m.Commit})
	}
	ws, err := b.Collections.CreateReserved(ctx, c.ID, spec)
	if err != nil {
		return err
	}
	for i, m := range ws.Members {
		for _, worker := range workers {
			if err = b.Collections.MergeReviewed(ctx, m, p.Baseline[i].Commit, worker.heads[m.Name]); err != nil {
				return fmt.Errorf("merge %s: %w", m.Name, err)
			}
		}
	}
	c.Members, err = b.planBaseline(ctx, c.ID)
	if err != nil {
		return err
	}
	_, _, err = b.Work.Append(mission, work.Draft{Type: work.CandidateReady, Subject: c.ID, Actor: work.Actor{Kind: work.ActorLazybox}, Payload: *c})
	if err != nil {
		return err
	}
	cmd, err := b.Commands.Start(commands.Spec{CollectionID: c.ID, Kind: "integration", Title: "Mission combined check", Argv: c.Check, Cwd: ws.Root, Env: b.runtimeEnv(ws, WorkspaceMember{}, 0, 0, nil)})
	if err != nil {
		return err
	}
	return b.observeCandidate(ctx, c, cmd)
}

func (b *Bench) observeCandidate(ctx context.Context, c *work.Candidate, cmd *commands.Command) error {
	c.Command = cmd.Status().ID
	status, err := cmd.Wait(ctx)
	if err != nil {
		return err
	}
	if status.ExitCode == nil || *status.ExitCode != 0 || status.Error != "" || status.Signal != "" {
		return fmt.Errorf("combined check %s failed; inspect its command output", c.Command)
	}
	after, err := b.planBaseline(ctx, c.ID)
	if err != nil {
		return err
	}
	if !slices.Equal(after, c.Members) {
		return errors.New("candidate revisions changed during the check")
	}
	return nil
}
func (b *Bench) PromoteCandidate(ctx context.Context, mission, id string) (*work.State, error) {
	unlock := b.orch().lock("integration/" + mission)
	defer unlock()
	s, err := b.Work.Mission(mission)
	if err != nil {
		return nil, err
	}
	c := s.Candidate(id)
	if c == nil {
		return nil, fmt.Errorf("%w: unknown candidate", work.ErrRefused)
	}
	if c.State == "accepted" {
		return s, nil
	}
	if c.State != "passed" || c.Generation != s.Generation {
		return nil, fmt.Errorf("%w: candidate has not passed on the current generation", work.ErrRefused)
	}
	_, _, refs, err := b.integrationInputs(ctx, s, c.Plan)
	if err != nil {
		return nil, err
	}
	if !maps.Equal(refs, c.Reviews) {
		return nil, fmt.Errorf("%w: worker reviews changed", work.ErrRefused)
	}
	after, err := b.planBaseline(ctx, id)
	if err != nil {
		return nil, err
	}
	if !slices.Equal(after, c.Members) {
		return nil, fmt.Errorf("%w: tested candidate changed", work.ErrRefused)
	}
	heads := map[string]string{}
	for _, m := range after {
		heads[m.Name] = m.Commit
	}
	if err = b.Collections.RetainCandidate(ctx, id, heads); err != nil {
		return nil, err
	}
	s, _, err = b.Work.Append(mission, work.Draft{Type: work.CandidateAccepted, Subject: id, Actor: work.Human, Payload: work.Candidate{ID: id}})
	return s, err
}
