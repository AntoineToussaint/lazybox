package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
)

// The review loop, the Runner's half (reviews/loop.go is the pure half): it
// sends what the loop decided — the findings to the author in auto mode, the
// re-review to the reviewer when the author's turn changed files — records
// the person's controls, and carries questions between the two agents.
// Everything it decides is on the review, persisted before it acts, so a
// restarted Runner picks the loop up where it was.

// ErrNotInReview refuses `lazybox ask` from a conversation in no review.
var ErrNotInReview = errors.New("this conversation takes part in no review: `lazybox ask` is between the author and its reviewer")

// MaxAskWait bounds one WaitAgentAsk call.
const MaxAskWait = 60 * time.Second

// askNoteGrace is how long an answer waits for a waiting asker before it is
// sent into the asker's conversation instead.
const askNoteGrace = 3 * time.Second

// reviewAsks wakes agents waiting for an answer.
type reviewAsks struct {
	mu      sync.Mutex
	changed chan struct{}
	waiting map[string]int
}

func (b *Bench) asks() *reviewAsks {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.reviewAsks == nil {
		b.reviewAsks = &reviewAsks{changed: make(chan struct{}), waiting: map[string]int{}}
	}
	return b.reviewAsks
}

func (a *reviewAsks) broadcast() {
	a.mu.Lock()
	close(a.changed)
	a.changed = make(chan struct{})
	a.mu.Unlock()
}

// reviewAct does what a step of the loop decided.
func (b *Bench) reviewAct(workspace string, run reviews.Run, action reviews.Action) {
	if run.TemplateID == reviews.VerificationTemplate {
		b.verificationFlow.Lock()
		defer b.verificationFlow.Unlock()
	}
	if len(action.SendFindings) > 0 {
		if err := b.sendFindings(workspace, run, action.SendFindings); err != nil {
			b.stopLoop(workspace, run.ID, "Could not send the findings to "+firstOf(run.SourceLabel, "the author")+": "+err.Error())
			return
		}
	}
	if action.Rereview {
		if err := b.sendRereview(workspace, run); err != nil {
			b.stopLoop(workspace, run.ID, "Could not start round "+fmt.Sprint(run.Round)+": "+err.Error())
			return
		}
	}
	if action.Escalate != "" {
		b.escalate(workspace, run, action.Escalate)
	}
}

// stopLoop ends a loop on an error, says why, and tells the person.
func (b *Bench) stopLoop(workspace, id, why string) {
	var run reviews.Run
	_, err := b.Reviews.Update(workspace, func(rv *reviews.Review) error {
		for i := range rv.Runs {
			if r := &rv.Runs[i]; r.ID == id {
				r.StopReason, r.Phase, r.Detail = reviews.StopError, reviews.PhaseDone, why
				r.Timeline = append(r.Timeline, reviews.Entry{At: time.Now().Unix(), Kind: "escalated", Actor: "Lazybox", Text: why, Round: r.Round})
				run = *r
				return nil
			}
		}
		return errors.New("review gone")
	})
	if err == nil {
		b.escalate(workspace, run, why)
	}
}

// escalate puts the loop's question to the person in the author's
// conversation, where its review card is.
func (b *Bench) escalate(workspace string, run reviews.Run, why string) {
	if source, err := b.Get(run.SourceSession); err == nil {
		b.recordReviewNote(source, "review_escalated", run, why)
	}
}

// recordReviewNote is one quiet line about a review in a conversation.
func (b *Bench) recordReviewNote(e *Execution, kind string, run reviews.Run, text string) {
	head, err := b.latest(e)
	if err != nil || head.Archived || head.Engine.Snapshot().Closed {
		return
	}
	data, _ := json.Marshal(map[string]any{"review": run.ID, "workspace": head.CollectionID, "reviewerSession": run.ReviewerSession, "reviewerLabel": run.ReviewerLabel, "sourceLabel": run.SourceLabel})
	_ = b.Record(head, agent.Event{Type: kind, Text: text, Status: run.StopReason, RequestID: run.ID, Data: data})
}

// anchored reads where each finding points now, for the message.
func (b *Bench) anchored(workspace string, ids []string) ([]reviews.Anchored, error) {
	review, err := b.Reviews.Load(workspace)
	if err != nil {
		return nil, err
	}
	c, err := b.Collections.Get(workspace)
	if err != nil {
		return nil, err
	}
	members := map[string]string{}
	for _, m := range c.Members {
		members[m.Name] = m.Path
	}
	var out []reviews.Anchored
	for _, comment := range review.Comments {
		if !containsString(ids, comment.ID) {
			continue
		}
		anchor := reviews.Anchor{Freshness: reviews.Unknown}
		if root := members[comment.Member]; root != "" {
			if content, exists, err := gitinspect.ReadWorking(root, comment.Path); err == nil {
				anchor = reviews.Evaluate(comment, content, exists)
			}
		}
		out = append(out, reviews.Anchored{Comment: comment, Anchor: anchor})
	}
	return out, nil
}

func containsString(list []string, value string) bool {
	for _, v := range list {
		if v == value {
			return true
		}
	}
	return false
}

// roundLabel is "Round 2 of 3" for a loop of several rounds, else empty.
func roundLabel(run reviews.Run) string {
	if run.MaxRounds <= 1 || run.Round <= 0 {
		return ""
	}
	return fmt.Sprintf("Round %d of %d", run.Round, run.MaxRounds)
}

// joinDetail joins an event's labels that are not empty: "Round 2 of 3 · 1 finding open".
func joinDetail(parts ...string) string {
	var kept []string
	for _, p := range parts {
		if p = strings.TrimSpace(p); p != "" {
			kept = append(kept, p)
		}
	}
	return strings.Join(kept, " · ")
}

// sendFindings delivers findings to the author's conversation as one
// message, exactly as the tray would put them in its box, and marks them sent.
func (b *Bench) sendFindings(workspace string, run reviews.Run, ids []string) error {
	if run.TemplateID == reviews.VerificationTemplate {
		current, err := b.Reviews.Load(workspace)
		if err != nil {
			return err
		}
		for _, saved := range current.Runs {
			if saved.ID == run.ID {
				if saved.Round != run.Round || saved.Phase != reviews.PhaseTriage || !saved.Active() || saved.Paused {
					return nil
				}
				run = saved
				break
			}
		}
	}
	source, err := b.Get(run.SourceSession)
	if err != nil {
		return err
	}
	if source, err = b.latest(source); err != nil {
		return err
	}
	if source.Engine.Snapshot().Closed {
		return errors.New("the author's conversation has ended")
	}
	findings, err := b.anchored(workspace, ids)
	if err != nil || len(findings) == 0 {
		return err
	}
	operation := fmt.Sprintf("review-%s-findings-%d", strings.TrimPrefix(run.ID, "rv-"), run.Round)
	origin := &agent.Origin{Kind: agent.OriginReviewFindings, Summary: fmt.Sprintf("%s review · %s sent", firstOf(run.ReviewerLabel, "Agent"), plural(len(findings), "finding", "findings")), Detail: roundLabel(run), From: run.ReviewerLabel}
	message := reviews.FindingsMessage(findings)
	if run.TemplateID == reviews.VerificationTemplate {
		recipes, err := json.MarshalIndent(run.Verification, "", "  ")
		if err != nil {
			return err
		}
		message += "\n\nRecorded acceptance checks and execution receipts follow. Reproduce the failure, repair the behavior, and retain confirmed reproductions as regression tests in the repository's existing test suite. Explain the test path and invocation to the reviewer.\n\n" + string(recipes)
	}
	if err = b.sendLazyboxMessage(b.ctx, source, operation, message, origin); err != nil {
		return err
	}
	heads := map[string]string{}
	for _, f := range findings {
		if _, ok := heads[f.Member]; !ok {
			heads[f.Member] = b.memberHead(workspace, f.Member)
		}
	}
	title := b.executionTitle(b.ctx, source)
	if _, err = b.Reviews.SetDisposition(workspace, ids, reviews.Sent, reviews.Handover{Session: source.ID, Title: title, Heads: heads}); err != nil {
		return err
	}
	_, err = b.Reviews.Update(workspace, func(rv *reviews.Review) error {
		reviews.FindingsSent(rv, run.ID, ids, "Lazybox", run.SourceLabel, time.Now().Unix())
		return nil
	})
	return err
}

func (b *Bench) memberHead(workspace, member string) string {
	c, err := b.Collections.Get(workspace)
	if err != nil {
		return ""
	}
	for _, m := range c.Members {
		if m.Name == member {
			ctx, cancel := context.WithTimeout(b.ctx, 3*time.Second)
			defer cancel()
			head, _ := gitinspect.Git{}.Head(ctx, m.Path)
			return head
		}
	}
	return ""
}

// ReviewFindingsSent follows findings the person sent from the tray: their
// review now waits for the author's fix.
func (b *Bench) ReviewFindingsSent(workspace string, ids []string, to string) {
	if b.Reviews == nil {
		return
	}
	_, _ = b.Reviews.Update(workspace, func(rv *reviews.Review) error {
		for _, run := range rv.Runs {
			reviews.FindingsSent(rv, run.ID, ids, "You", to, time.Now().Unix())
		}
		return nil
	})
}

// turnDiff is how to read what one turn of a conversation changed.
func (b *Bench) turnDiff(conversation string, seq int, members []collections.Member) []string {
	if b.Checkpoints == nil {
		return nil
	}
	record, err := b.Checkpoints.List(conversation)
	if err != nil {
		return nil
	}
	var out []string
	for _, t := range record.Turns {
		if t.Seq != seq || t.After == nil {
			continue
		}
		before := map[string]string{}
		for _, r := range t.Before.Repos {
			before[r.Member] = r.Commit
		}
		for _, r := range t.After.Repos {
			for _, m := range members {
				if m.Name == r.Member && before[r.Member] != "" && r.Commit != "" && before[r.Member] != r.Commit {
					out = append(out, fmt.Sprintf("    git -C %s diff %s %s", shellQuote(m.Path), before[r.Member], r.Commit))
				}
			}
		}
	}
	return out
}

// sendRereview asks the reviewer for the current round, once.
func (b *Bench) sendRereview(workspace string, run reviews.Run) error {
	if run.TemplateID == reviews.VerificationTemplate {
		current, err := b.Reviews.Load(workspace)
		if err != nil {
			return err
		}
		for _, saved := range current.Runs {
			if saved.ID == run.ID {
				run = saved
				break
			}
		}
		if !run.Active() || run.Paused || run.Phase != reviews.PhaseReviewing || run.Rounds[len(run.Rounds)-1].Sent {
			return nil
		}
	}
	reviewer, err := b.Get(run.ReviewerSession)
	if err != nil {
		return err
	}
	if reviewer, err = b.latest(reviewer); err != nil {
		return err
	}
	if reviewer.Engine.Snapshot().Closed {
		return errors.New("the review conversation has ended")
	}
	review, err := b.Reviews.Load(workspace)
	if err != nil {
		return err
	}
	var open []reviews.Comment
	for _, c := range review.Comments {
		if c.ReviewID == run.ID && c.Live() && c.Delivered {
			open = append(open, c)
		}
	}
	var diff []string
	if c, err := b.Collections.Get(workspace); err == nil && len(run.Rounds) > 0 {
		diff = b.turnDiff(run.SourceConversation, run.Rounds[len(run.Rounds)-1].AuthorTurn, c.Members)
	}
	if run.TemplateID == reviews.VerificationTemplate {
		collection, captureErr := b.Collections.Get(workspace)
		if captureErr != nil {
			return captureErr
		}
		if len(run.Rounds[len(run.Rounds)-1].Input) == 0 {
			scope, captureErr := b.captureReviewInput(b.ctx, conversationOf(reviewer), collection, run.Compare)
			if captureErr != nil {
				return captureErr
			}
			run, err = b.Reviews.UpdateRun(workspace, run.ID, func(r *reviews.Run) error {
				r.Input = scope.input
				r.Rounds[len(r.Rounds)-1].Input = scope.input
				return nil
			})
			if err != nil {
				return err
			}
		}
		diff = nil
		for _, input := range run.Input {
			for _, member := range collection.Members {
				if member.Name == input.Member {
					diff = append(diff, fmt.Sprintf("git -C %s diff %s %s --", shellQuote(member.Path), input.Base, input.Commit))
				}
			}
		}

		for _, check := range run.Verification {
			if _, err := b.startVerification(b.ctx, reviewer, check); err != nil {
				return err
			}
		}
		diff = append(diff, "Recheck every recorded acceptance criterion. Wait for the new verification commands, inspect their output, and resolve findings only after the relevant checks pass. Propose durable regression tests to the author for confirmed failures.", reviewCompletionInstructions(run.TemplateID))
	}
	operation := fmt.Sprintf("review-%s-round-%d", strings.TrimPrefix(run.ID, "rv-"), run.Round)
	origin := &agent.Origin{Kind: agent.OriginReviewRound, Summary: fmt.Sprintf("Re-review asked · %s changed files", firstOf(run.SourceLabel, "the author")), Detail: joinDetail(roundLabel(run), plural(len(open), "finding", "findings")+" open"), From: run.SourceLabel}
	if err = b.sendLazyboxMessage(b.ctx, reviewer, operation, reviews.RereviewMessage(run, open, diff), origin); err != nil {
		return err
	}
	_, err = b.Reviews.UpdateRun(workspace, run.ID, func(r *reviews.Run) error {
		if n := len(r.Rounds); n > 0 {
			r.Rounds[n-1].Sent = true
		}
		return nil
	})
	if source, err := b.Get(run.SourceSession); err == nil {
		b.recordReviewEvent(source, "review_started", run, reviews.CountFindings(review, run.ID))
	}
	return err
}

// authorTurnEnded follows the author's turns for its active loops: a turn
// that began after the findings were sent and changed files starts the next
// round.
func (b *Bench) authorTurnEnded(e *Execution, operation string, matches []reviews.Match) {
	if operation == "" || b.Checkpoints == nil {
		return
	}
	var seq, files int
	var started int64
	found := false
	if record, err := b.Checkpoints.List(conversationOf(e)); err == nil {
		for _, t := range record.Turns {
			if t.OperationID == operation && t.After != nil {
				seq, files, started, found = t.Seq, t.Files, t.Before.At.Unix(), true
			}
		}
	}
	if !found {
		return
	}
	for _, m := range matches {
		if m.Role != reviews.RoleAuthor || !m.Run.Active() {
			continue
		}
		var action reviews.Action
		var run reviews.Run
		_, err := b.Reviews.Update(m.Workspace, func(rv *reviews.Review) error {
			var err error
			if action, err = reviews.AuthorTurnEnded(rv, m.Run.ID, seq, started, files, time.Now().Unix()); err != nil {
				return err
			}
			for _, r := range rv.Runs {
				if r.ID == m.Run.ID {
					run = r
				}
			}
			return nil
		})
		if err == nil {
			b.reviewAct(m.Workspace, run, action)
		}
	}
}

// ReviewVerdict is the reviewer's resolve or reopen of one of its findings.
func (b *Bench) ReviewVerdict(session, finding, state, note string) (reviews.Comment, error) {
	if b.Reviews == nil {
		return reviews.Comment{}, errors.New("reviews are not available on this Runner")
	}
	e, err := b.Get(session)
	if err != nil {
		return reviews.Comment{}, err
	}
	m, ok := b.Reviews.ReviewerRun(e.ID, conversationOf(e))
	if !ok || m.Workspace != e.CollectionID {
		return reviews.Comment{}, ErrNotReviewer
	}
	var out reviews.Comment
	_, err = b.Reviews.Update(m.Workspace, func(rv *reviews.Review) (err error) {
		out, err = reviews.Verdict(rv, m.Run.ID, finding, state, note, time.Now().Unix())
		return
	})
	return out, err
}

// ControlReview is the person's control of a review's loop.
func (b *Bench) ControlReview(workspace, id, action, mode string, rounds int) (reviews.Run, error) {
	if b.Reviews == nil {
		return reviews.Run{}, errors.New("reviews are not available on this Runner")
	}
	var run reviews.Run
	_, err := b.Reviews.Update(workspace, func(rv *reviews.Review) error {
		if err := reviews.Control(rv, id, action, mode, rounds, time.Now().Unix()); err != nil {
			return err
		}
		for _, r := range rv.Runs {
			if r.ID == id {
				run = r
			}
		}
		return nil
	})
	return run, err
}

// ---- Ending a review by hand -------------------------------------------------

// CancelReview is the person's cancel of a review under way (GITHUB.md,
// "Cancelling a review"). The cancel is recorded first, so no round starts
// after it whatever the reviewer's turn does next; then the reviewer's
// running turn is interrupted and its session ended. Its conversation and
// the findings it filed are kept. workspace may be empty: the review's own.
// It returns the review's workspace and the review as cancelled.
func (b *Bench) CancelReview(ctx context.Context, workspace, id, by string) (string, reviews.Run, error) {
	if b.Reviews == nil {
		return "", reviews.Run{}, errors.New("reviews are not available on this Runner")
	}
	if workspace == "" {
		var err error
		if workspace, err = b.reviewWorkspace(id); err != nil {
			return "", reviews.Run{}, err
		}
	}
	var run reviews.Run
	already := false
	_, err := b.Reviews.Update(workspace, func(rv *reviews.Review) (err error) {
		run, err = reviews.Cancel(rv, id, by, time.Now().Unix())
		already = errors.Is(err, reviews.Unchanged)
		return err
	})
	if err != nil {
		return workspace, run, err
	}
	if !already {
		b.asks().broadcast()
		b.reviewEndedByHand(workspace, run)
	}
	return workspace, run, b.endReviewer(ctx, run)
}

// reviewWorkspace is the workspace that keeps a review.
func (b *Bench) reviewWorkspace(id string) (string, error) {
	for _, w := range b.reviewWorkspaces() {
		review, err := b.Reviews.Load(w)
		if err != nil {
			continue
		}
		for _, r := range review.Runs {
			if r.ID == id {
				return w, nil
			}
		}
	}
	return "", fmt.Errorf("review %q not found: %w", id, os.ErrNotExist)
}

// endReviewer interrupts a cancelled review's reviewer if it is mid-turn,
// then ends its session as End session does: its conversation stays, and a
// new session can continue it.
func (b *Bench) endReviewer(ctx context.Context, run reviews.Run) error {
	e, err := b.Get(run.ReviewerSession)
	if err != nil {
		return nil // already gone: nothing runs
	}
	if latest, err := b.latest(e); err == nil {
		e = latest
	}
	s := e.Engine.Snapshot()
	if s.Closed {
		return nil
	}
	if s.Busy {
		stop, cancel := context.WithTimeout(ctx, 10*time.Second)
		// Best effort: ending the session below stops the turn regardless.
		_, _ = e.Engine.Apply(stop, agent.Operation{ID: "review-cancel-" + strings.TrimPrefix(run.ID, "rv-"), Kind: agent.Interrupt})
		cancel()
	}
	if err = b.StopExecution(ctx, e.ID); err != nil {
		return fmt.Errorf("the review is cancelled, but %s's session could not be ended: %w", firstOf(run.ReviewerLabel, "the reviewer"), err)
	}
	return nil
}

// ReviewerRuns are the reviews session's conversation is the reviewer of.
func (b *Bench) ReviewerRuns(session string) []reviews.Match {
	if b.Reviews == nil {
		return nil
	}
	ids := []string{session}
	if e, err := b.Get(session); err == nil {
		ids = append(ids, conversationOf(e))
	}
	var out []reviews.Match
	for _, m := range b.Reviews.RunsOf(ids...) {
		if m.Role == reviews.RoleReviewer {
			out = append(out, m)
		}
	}
	return out
}

// ReviewerEndedByHand follows the person ending a conversation by hand —
// reviews.EndedInterrupted (Stop), EndedSession (End session) or
// EndedDeleted — for the reviews it was the reviewer of (ReviewerRuns,
// read before a delete): one under way stops, said on its card, rather than
// wait on a reviewer that will not report.
func (b *Bench) ReviewerEndedByHand(matches []reviews.Match, how string) {
	for _, m := range matches {
		var run reviews.Run
		_, err := b.Reviews.Update(m.Workspace, func(rv *reviews.Review) (err error) {
			run, err = reviews.ReviewerEnded(rv, m.Run.ID, how, "You", time.Now().Unix())
			if errors.Is(err, reviews.Unchanged) {
				run = reviews.Run{}
			}
			return err
		})
		if err != nil || run.ID == "" {
			continue
		}
		b.asks().broadcast()
		b.reviewEndedByHand(m.Workspace, run)
	}
}

// reviewEndedByHand tells the author's conversation, and the mission task
// the review is evidence on, that the person ended the review.
func (b *Bench) reviewEndedByHand(workspace string, run reviews.Run) {
	b.cancelVerification(run)
	review, err := b.Reviews.Load(workspace)
	if err != nil {
		return
	}
	counts := reviews.CountFindings(review, run.ID)
	if source, err := b.Get(run.SourceSession); err == nil {
		b.recordReviewEvent(source, "review_finished", run, counts)
	}
	b.recordReviewEvidence(run, counts, true)
}

// ---- Questions between the two agents --------------------------------------

// AskRequest is one `lazybox ask`.
type AskRequest struct {
	Session, To, Question string
	Wait                  time.Duration
}

// AskAgent records a question from session to the other side of its review
// and delivers it there as a labelled message. With Wait it waits for the
// reply, at most MaxAskWait.
func (b *Bench) AskAgent(ctx context.Context, req AskRequest) (reviews.Ask, error) {
	if b.Reviews == nil {
		return reviews.Ask{}, errors.New("reviews are not available on this Runner")
	}
	question, err := reviews.CleanQuestion(req.Question)
	if err != nil {
		return reviews.Ask{}, err
	}
	e, err := b.Get(req.Session)
	if err != nil {
		return reviews.Ask{}, err
	}
	matches := b.Reviews.RunsOf(e.ID, conversationOf(e))
	var m *reviews.Match
	to := strings.ToLower(strings.TrimSpace(req.To))
	for i := range matches {
		x := matches[i]
		if x.Workspace != e.CollectionID {
			continue
		}
		other, otherConversation := x.Run.SourceSession, x.Run.SourceConversation
		if x.Role == reviews.RoleAuthor {
			other, otherConversation = x.Run.ReviewerSession, x.Run.ReviewerConversation
		}
		switch {
		case to == "author" && x.Role == reviews.RoleReviewer, to == "reviewer" && x.Role == reviews.RoleAuthor, to != "" && (to == other || to == otherConversation):
			m = &x
		}
		if m != nil {
			break
		}
	}
	if m == nil {
		if len(matches) == 0 {
			return reviews.Ask{}, ErrNotInReview
		}
		return reviews.Ask{}, fmt.Errorf("%w: ask \"author\" (as the reviewer) or \"reviewer\" (as the author) of a review in this workspace", reviews.ErrAskRefused)
	}
	a := reviews.Ask{FromSession: e.ID, Question: question}
	if m.Role == reviews.RoleReviewer {
		a.From, a.To, a.ToSession, a.FromLabel, a.ToLabel = reviews.RoleReviewer, reviews.RoleAuthor, m.Run.SourceSession, m.Run.ReviewerLabel, m.Run.SourceLabel
	} else {
		a.From, a.To, a.ToSession, a.FromLabel, a.ToLabel = reviews.RoleAuthor, reviews.RoleReviewer, m.Run.ReviewerSession, m.Run.SourceLabel, m.Run.ReviewerLabel
	}
	target, err := b.Get(a.ToSession)
	if err == nil {
		target, err = b.latest(target)
	}
	if err != nil || target.Engine.Snapshot().Closed {
		return reviews.Ask{}, fmt.Errorf("%w: %s's conversation has ended", reviews.ErrAskRefused, a.ToLabel)
	}
	a.ToSession = target.ID
	_, err = b.Reviews.Update(m.Workspace, func(rv *reviews.Review) (err error) {
		a, err = reviews.NewAsk(rv, m.Run.ID, a, time.Now().Unix())
		return
	})
	if err != nil {
		return reviews.Ask{}, err
	}
	role := "reviewing your work"
	if a.From == reviews.RoleAuthor {
		role = "whose work you are reviewing"
	}
	text := fmt.Sprintf("[Lazybox] %s (%s) asks you:\n\n%s\n\nReply in your next message: Lazybox passes your reply back to %s. Answer the question; do not start other work in this reply.", a.FromLabel, role, quoteText(question), a.FromLabel)
	line := fmt.Sprintf("%s asked %s: %s", a.FromLabel, a.ToLabel, question)
	origin := &agent.Origin{Kind: agent.OriginAgentAsk, Summary: line, From: a.FromLabel}
	if err = b.sendLazyboxMessage(ctx, target, a.Operation, text, origin); err != nil {
		_, _ = b.Reviews.Update(m.Workspace, func(rv *reviews.Review) error {
			reviews.AnswerAsk(rv, m.Run.ID, a.ToSession, a.Operation, "(The question could not be delivered: "+err.Error()+")", time.Now().Unix())
			return nil
		})
		return reviews.Ask{}, err
	}
	b.recordExchange(e, m.Run, line)
	b.recordExchange(target, m.Run, line)
	if req.Wait <= 0 {
		return a, nil
	}
	return b.WaitAgentAsk(ctx, a.ID, req.Wait)
}

func quoteText(text string) string {
	lines := strings.Split(text, "\n")
	for i, l := range lines {
		lines[i] = strings.TrimRight("> "+l, " ")
	}
	return strings.Join(lines, "\n")
}

// recordExchange shows one line of an exchange between the agents.
func (b *Bench) recordExchange(e *Execution, run reviews.Run, line string) {
	head, err := b.latest(e)
	if err != nil || head.Archived || head.Engine.Snapshot().Closed {
		return
	}
	data, _ := json.Marshal(map[string]any{"review": run.ID, "workspace": head.CollectionID, "reviewerSession": run.ReviewerSession, "sourceSession": run.SourceSession})
	_ = b.Record(head, agent.Event{Type: "review_exchange", Text: line, RequestID: run.ID, Data: data})
}

// askLocation finds the review holding a question.
func (b *Bench) askLocation(id string) (string, reviews.Run, error) {
	for _, m := range b.reviewWorkspaces() {
		review, err := b.Reviews.Load(m)
		if err != nil {
			continue
		}
		if run, ok := reviews.AskRun(review, id); ok {
			return m, run, nil
		}
	}
	return "", reviews.Run{}, fmt.Errorf("question %q: %w", id, reviews.ErrNotFound)
}

// reviewWorkspaces are the workspaces this Runner keeps.
func (b *Bench) reviewWorkspaces() []string {
	if b.Collections == nil {
		return nil
	}
	list := b.Collections.List()
	out := make([]string, 0, len(list))
	for _, c := range list {
		out = append(out, c.ID)
	}
	return out
}

// AskSession is the execution that asked a question (the agent allowlist).
func (b *Bench) AskSession(id string) (string, error) {
	workspace, run, err := b.askLocation(id)
	if err != nil {
		return "", err
	}
	review, err := b.Reviews.Load(workspace)
	if err != nil {
		return "", err
	}
	a, err := reviews.FindAsk(&review, run.ID, id, time.Now().Unix())
	return a.FromSession, err
}

// WaitAgentAsk waits, at most wait (bounded), for a question's reply. A
// reply collected here is the asker's: no note is sent for it.
func (b *Bench) WaitAgentAsk(ctx context.Context, id string, wait time.Duration) (reviews.Ask, error) {
	wait = min(max(wait, 0), MaxAskWait)
	workspace, run, err := b.askLocation(id)
	if err != nil {
		return reviews.Ask{}, err
	}
	s := b.asks()
	s.mu.Lock()
	s.waiting[id]++
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		if s.waiting[id]--; s.waiting[id] <= 0 {
			delete(s.waiting, id)
		}
		s.mu.Unlock()
	}()
	deadline := time.NewTimer(wait)
	defer deadline.Stop()
	for {
		s.mu.Lock()
		changed := s.changed
		s.mu.Unlock()
		var a reviews.Ask
		_, err := b.Reviews.Update(workspace, func(rv *reviews.Review) (err error) {
			if a, err = reviews.FindAsk(rv, run.ID, id, time.Now().Unix()); err != nil {
				return err
			}
			if a.State == reviews.AskAnswered {
				if told, ok := reviews.TellAsk(rv, run.ID, id); ok {
					a = told
				}
			}
			return nil
		})
		if err != nil || a.State != reviews.AskPending {
			return a, err
		}
		select {
		case <-changed:
		case <-deadline.C:
			return a, nil
		case <-ctx.Done():
			return a, ctx.Err()
		case <-b.ctx.Done():
			return a, errors.New("the Runner is stopping; wait again in a moment")
		}
	}
}

// answerAsk takes a turn of e that carried a question as its reply: the
// asker gets it (waiting, or as a note in its conversation), and both
// conversations show it. It reports the turn was an answer.
func (b *Bench) answerAsk(e *Execution, operation string, matches []reviews.Match) bool {
	answer := e.Engine.Snapshot().LastMessage
	for _, m := range matches {
		if operation == "" && m.Run.Phase == reviews.PhaseReviewing && m.Role == reviews.RoleReviewer {
			continue // an adopted reviewer turn mid-round is the round, not an answer
		}
		var a reviews.Ask
		var ok bool
		_, err := b.Reviews.Update(m.Workspace, func(rv *reviews.Review) error {
			a, ok = reviews.AnswerAsk(rv, m.Run.ID, e.ID, operation, answer, time.Now().Unix())
			if !ok {
				return reviews.Unchanged
			}
			return nil
		})
		if err != nil || !ok {
			continue
		}
		b.asks().broadcast()
		line := fmt.Sprintf("%s answered %s: %s", a.ToLabel, a.FromLabel, a.Answer)
		b.recordExchange(e, m.Run, line)
		if asker, err := b.Get(a.FromSession); err == nil {
			b.recordExchange(asker, m.Run, line)
		}
		b.askNoteLater(m.Workspace, m.Run.ID, a)
		return true
	}
	return false
}

// askNoteLater sends the reply into the asker's conversation unless a wait
// collects it first.
func (b *Bench) askNoteLater(workspace, runID string, a reviews.Ask) {
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		timer := time.NewTimer(askNoteGrace)
		defer timer.Stop()
		select {
		case <-timer.C:
		case <-b.ctx.Done():
			return
		}
		s := b.asks()
		s.mu.Lock()
		waiting := s.waiting[a.ID] > 0
		s.mu.Unlock()
		if waiting {
			return
		}
		var told reviews.Ask
		var ok bool
		_, _ = b.Reviews.Update(workspace, func(rv *reviews.Review) error {
			if told, ok = reviews.TellAsk(rv, runID, a.ID); !ok {
				return reviews.Unchanged
			}
			return nil
		})
		if !ok {
			return
		}
		asker, err := b.Get(told.FromSession)
		if err != nil {
			return
		}
		if asker, err = b.latest(asker); err != nil || asker.Engine.Snapshot().Closed {
			return
		}
		note := fmt.Sprintf("[Lazybox] %s answered your question (%s):\n\n%s", told.ToLabel, quoteText(told.Question), told.Answer)
		ctx, cancel := context.WithTimeout(b.ctx, 10*time.Second)
		defer cancel()
		origin := &agent.Origin{Kind: agent.OriginAgentAnswer, Summary: fmt.Sprintf("%s answered %s: %s", told.ToLabel, told.FromLabel, told.Answer), From: told.ToLabel}
		_ = b.sendLazyboxMessage(ctx, asker, "review-answer-"+strings.TrimPrefix(told.ID, "ask-"), note, origin)
	}()
}
