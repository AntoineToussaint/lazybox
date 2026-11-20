package runner

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
)

// startVerification admits a recipe once and retains its receipts before the
// reviewer can claim success. A retry returns the existing round's commands.
func (b *Bench) startVerification(ctx context.Context, e *Execution, check reviews.VerificationCheck) (reviews.VerificationCheck, error) {
	if b.Reviews == nil || b.Checkpoints == nil || b.Commands == nil {
		return check, errors.New("verification is unavailable")
	}
	if err := check.Validate(); err != nil {
		return check, err
	}
	match, ok := b.Reviews.ReviewerRun(e.ID, conversationOf(e))
	if !ok || match.Workspace != e.CollectionID || match.Run.TemplateID != reviews.VerificationTemplate {
		return check, ErrNotReviewer
	}
	lock, _ := b.verificationLaunch.LoadOrStore(match.Run.ID, &sync.Mutex{})
	lock.(*sync.Mutex).Lock()
	defer lock.(*sync.Mutex).Unlock()
	// Re-read after waiting for another launch.
	match, ok = b.Reviews.ReviewerRun(e.ID, conversationOf(e))
	if !ok {
		return check, ErrNotReviewer
	}
	workspace, err := b.Collections.Get(match.Workspace)
	if err != nil {
		return check, err
	}
	member := slices.IndexFunc(workspace.Members, func(m collections.Member) bool { return m.Name == check.Member && m.Ready })
	input := slices.IndexFunc(match.Run.Input, func(i reviews.ReviewInput) bool { return i.Member == check.Member })
	if member < 0 || input < 0 {
		return check, errors.New("verification repository is outside the captured input")
	}
	captured := match.Run.Input[input]

	run, err := b.Reviews.UpdateRun(match.Workspace, match.Run.ID, func(r *reviews.Run) error {
		if !r.Active() || r.Paused || r.Phase != reviews.PhaseReviewing || r.Round != match.Run.Round || !slices.Equal(r.Input, match.Run.Input) || len(r.Rounds) == 0 || !slices.Equal(r.Rounds[len(r.Rounds)-1].Input, r.Input) || len(r.Rounds[len(r.Rounds)-1].Input) == 0 {
			return errors.New("verification review is not running")
		}
		index := slices.IndexFunc(r.Verification, func(c reviews.VerificationCheck) bool { return c.Name == check.Name })
		if index >= 0 {
			saved := r.Verification[index]
			if saved.Member != check.Member || saved.Expected != check.Expected || saved.Regression != check.Regression || !slices.Equal(saved.Argv, check.Argv) {
				return errors.New("an existing acceptance check cannot be changed; add a new criterion")
			}
			if len(saved.Runs) > 0 && saved.Runs[len(saved.Runs)-1].Round == r.Round {
				result := saved.Runs[len(saved.Runs)-1]
				if result.Base != captured.Base || result.Candidate != captured.Commit || result.IsolationVersion != 1 {
					return errors.New("stale verification receipt; start a new review")
				}
				return nil
			}
		} else {
			if len(r.Verification) >= 30 {
				return errors.New("a review supports at most 30 acceptance checks")
			}
			r.Verification = append(r.Verification, check)
			index = len(r.Verification) - 1
		}
		r.Verification[index].Runs = append(r.Verification[index].Runs, reviews.VerificationExecution{IsolationVersion: 1, Round: r.Round, Base: captured.Base, Candidate: captured.Commit, State: "preparing"})
		return nil
	})
	if err != nil {
		return check, err
	}
	index := slices.IndexFunc(run.Verification, func(c reviews.VerificationCheck) bool { return c.Name == check.Name })
	b.refreshVerification(&run)
	result := run.Verification[index].Runs[len(run.Verification[index].Runs)-1]
	for _, side := range []string{"base", "candidate"} {
		if (side == "base" && result.BaseCommand != "") || (side == "candidate" && result.CandidateCommand != "") {
			continue
		}
		revision := result.Base
		if side == "candidate" {
			revision = result.Candidate
		}
		dir, startErr := b.Checkpoints.VerificationCheckout(ctx, conversationOf(e), workspace.Members[member].Path, revision)
		if startErr == nil {
			var command *commands.Command
			e.activity.Lock()
			if e.Engine != nil {
				state := e.Engine.Snapshot()
				if state.Closed || state.Fault != "" || state.Status == "switching" {
					e.activity.Unlock()
					return check, errors.New("reviewer execution is unavailable")
				}
			}
			command, startErr = b.Commands.Start(commands.Spec{SessionID: e.ID, Kind: "verification", Action: fmt.Sprintf("%s/%d/%s/%s", run.ID, run.Round, check.Name, side), Title: "Verify " + check.Name + " (" + side + ")", Cwd: dir, Argv: check.Argv})
			e.activity.Unlock()
			if startErr == nil {
				if side == "base" {
					result.BaseCommand = command.Status().ID
				} else {
					result.CandidateCommand = command.Status().ID
				}
			}
		}
		if startErr != nil {
			result.State, result.Detail = "unverified", startErr.Error()
		} else {
			result.State = "running"
		}
		_, err = b.Reviews.UpdateRun(match.Workspace, run.ID, func(r *reviews.Run) error {
			if !r.Active() || r.Paused || r.Round != run.Round || r.Phase != reviews.PhaseReviewing {
				return errors.New("verification round ended while starting the check")
			}
			r.Verification[index].Runs[len(r.Verification[index].Runs)-1] = result
			return nil
		})
		if err != nil {
			b.cancelVerification(run)
			return check, err
		}
		if startErr != nil {
			break
		}
	}
	check = run.Verification[index]
	check.Runs[len(check.Runs)-1] = result
	return check, nil
}

// refreshVerification uses Runner-owned command status, never the agent's
// claimed exit code. Interrupted starts remain unverified after a restart.
func (b *Bench) refreshVerification(run *reviews.Run) {
	for i := range run.Verification {
		check := &run.Verification[i]
		if len(check.Runs) == 0 {
			continue
		}
		result := &check.Runs[len(check.Runs)-1]
		if result.Round != run.Round || result.IsolationVersion != 1 {
			result.State, result.Detail = "unverified", "Receipt predates isolated, revision-bound verification"
			continue
		}
		input := slices.IndexFunc(run.Input, func(i reviews.ReviewInput) bool { return i.Member == check.Member })
		if input < 0 || result.Base != run.Input[input].Base || result.Candidate != run.Input[input].Commit {
			result.State, result.Detail = "unverified", "Receipt does not match captured revisions"
			continue
		}
		if result.BaseCommand == "" || result.CandidateCommand == "" {
			prefix := fmt.Sprintf("%s/%d/%s/", run.ID, result.Round, check.Name)
			for _, status := range b.Commands.List("") {
				if status.Spec.Kind != "verification" || !status.Spec.Isolated {
					continue
				}
				if status.Spec.Action == prefix+"base" {
					result.BaseCommand = status.ID
				}
				if status.Spec.Action == prefix+"candidate" {
					result.CandidateCommand = status.ID
				}
			}
		}
		if result.BaseCommand == "" || result.CandidateCommand == "" {
			result.State = "unverified"
			continue
		}
		base, err := b.Commands.Get(result.BaseCommand)
		if err != nil {
			result.State, result.Detail = "unverified", err.Error()
			continue
		}
		candidate, err := b.Commands.Get(result.CandidateCommand)
		if err != nil {
			result.State, result.Detail = "unverified", err.Error()
			continue
		}
		bs, cs := base.Status(), candidate.Status()
		if !bs.Spec.Isolated || !cs.Spec.Isolated {
			result.State = "unverified"
			continue
		}
		if !bs.Done || !cs.Done {
			result.State = "running"
			continue
		}
		result.BaseExit, result.CandidateExit = bs.ExitCode, cs.ExitCode
		result.State = "unverified"
		if bs.Signal != "" || cs.Signal != "" || bs.Error != "" || cs.Error != "" || bs.ExitCode == nil || cs.ExitCode == nil {
			result.Detail = "Execution did not produce complete exit results"
			continue
		}
		result.State = "failed"
		if *cs.ExitCode == 0 {
			result.State = "passed"
		}
	}
}

// RunReviewVerification is the CLI entry point for a reviewer's acceptance check.
func (b *Bench) RunReviewVerification(ctx context.Context, session string, check reviews.VerificationCheck) (reviews.VerificationCheck, error) {
	e, err := b.Get(session)
	if err != nil {
		return check, err
	}
	return b.startVerification(ctx, e, check)
}

// cancelVerification also covers a command whose ID had not yet been saved
// when cancellation or a Runner restart interrupted its start.
func (b *Bench) cancelVerification(run reviews.Run) {
	if run.TemplateID != reviews.VerificationTemplate || b.Commands == nil {
		return
	}
	for _, status := range b.Commands.List("") {
		if status.Spec.Kind == "verification" && strings.HasPrefix(status.Spec.Action, run.ID+"/") && !status.Done {
			if command, err := b.Commands.Get(status.ID); err == nil {
				_ = command.Cancel()
			}
		}
	}
}

// watchVerification resumes undelivered round transitions and bounds test
// execution across Runner restarts using the managed command's saved start.
func (b *Bench) watchVerification() {
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		ticker := time.NewTicker(5 * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-b.ctx.Done():
				return
			case <-ticker.C:
			}
			for _, workspace := range b.reviewWorkspaces() {
				review, err := b.Reviews.Load(workspace)
				if err != nil {
					continue
				}
				for _, run := range review.Runs {
					if run.TemplateID != reviews.VerificationTemplate {
						continue
					}
					if !run.Active() {
						b.reconcileVerificationEvidence(workspace, run, review.Comments)
						b.cancelVerification(run)
						continue
					}
					if run.Paused {
						continue
					}
					if run.Phase == reviews.PhaseReviewing && len(run.Rounds) > 0 && !run.Rounds[len(run.Rounds)-1].Sent {
						b.reviewAct(workspace, run, reviews.Action{Rereview: true})
					}
					if run.Phase == reviews.PhaseTriage && run.Loop == reviews.LoopAuto {
						var ids []string
						for _, finding := range review.Comments {
							if finding.ReviewID == run.ID && (finding.Disposition == reviews.Proposed || finding.Disposition == reviews.Open) {
								ids = append(ids, finding.ID)
							}
						}
						if len(ids) > 0 {
							b.reviewAct(workspace, run, reviews.Action{SendFindings: ids})
						}
					}
				}
			}
		}
	}()
}

// Reconcile terminal evidence from older Runners; artifact history remains
// available, while its latest version and mission evidence retract false passes.
func (b *Bench) reconcileVerificationEvidence(workspace string, run reviews.Run, comments []reviews.Comment) {
	b.refreshVerification(&run)
	if b.Work != nil && (run.StopReason != reviews.StopClean || !reviews.VerificationComplete(run)) {
		_ = b.Work.InvalidateReview("review:"+run.ID, "Acceptance verification is incomplete; previous passing evidence was invalidated.")
	}

	if run.StopReason == reviews.StopClean && !reviews.VerificationComplete(run) {
		_, err := b.Reviews.UpdateRun(workspace, run.ID, func(saved *reviews.Run) error {
			if saved.Active() {
				return reviews.Unchanged
			}
			b.refreshVerification(saved)
			if reviews.VerificationComplete(*saved) {
				return reviews.Unchanged
			}
			saved.StopReason, saved.Detail = reviews.StopUnverified, "Previous evidence did not verify these revisions in isolation. Start a new review."
			return b.publishReviewVersion(workspace, *saved, comments, "Previous verification invalidated; original report retained in artifact history.", saved.Detail, fmt.Sprintf("review-round-%d-invalidated-v1", saved.Round))
		})
		if err != nil {
			return
		}
	}
}
