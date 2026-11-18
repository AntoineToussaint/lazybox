package transport

import (
	"bytes"
	"context"
	"errors"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Inline review comments. The Runner keeps them per workspace and anchors
// each one against the member's working tree on every read: still at its
// lines, moved with them, or outdated. A comment is resolved on its own only
// when the person asked for that, after it was sent, and once a commit
// changed exactly what it quotes. Owner only, like every RPC off the agent
// allowlist.

func (s *Service) reviewStore(workspace string) (*reviews.Store, error) {
	if s.Bench.Reviews == nil {
		return nil, grpcError(codes.FailedPrecondition, errors.New("review comments are not available on this Runner"))
	}
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	if _, err = manager.Get(workspace); err != nil {
		return nil, rpcError(err)
	}
	return s.Bench.Reviews, nil
}

// memberRoots resolves and caches each member's worktree for one request.
type memberRoots struct {
	s         *Service
	workspace string
	roots     map[string]string
	errs      map[string]error
	heads     map[string]string
}

func (s *Service) memberRoots(workspace string) *memberRoots {
	return &memberRoots{s: s, workspace: workspace, roots: map[string]string{}, errs: map[string]error{}, heads: map[string]string{}}
}

func (m *memberRoots) root(ctx context.Context, member string) (string, error) {
	if root, ok := m.roots[member]; ok {
		return root, m.errs[member]
	}
	manager, err := m.s.collectionStore()
	root := ""
	if err == nil {
		root, err = manager.InspectionPath(ctx, m.workspace, member)
	}
	m.roots[member], m.errs[member] = root, err
	return root, err
}

func (m *memberRoots) head(ctx context.Context, member string) string {
	if head, ok := m.heads[member]; ok {
		return head
	}
	head := ""
	if root, err := m.root(ctx, member); err == nil {
		head, _ = gitinspect.Git{}.Head(ctx, root)
	}
	m.heads[member] = head
	return head
}

// committedAway reports whether a commit after the handover changed what a
// sent comment quotes: HEAD moved, that file differs between the two commits,
// and the quoted lines are in neither the new commit nor the working tree.
func committedAway(ctx context.Context, root, head string, c reviews.Comment, anchor reviews.Anchor) bool {
	if c.Side != reviews.SideNew || anchor.Freshness != reviews.Outdated || c.SentHead == "" || head == "" || head == c.SentHead {
		return false
	}
	git := gitinspect.Git{}
	before, hadIt, err := git.ShowAt(ctx, root, c.SentHead, c.Path)
	if err != nil {
		return false
	}
	after, hasIt, err := git.ShowAt(ctx, root, head, c.Path)
	if err != nil || (hadIt == hasIt && bytes.Equal(before, after)) {
		return false
	}
	return !hasIt || reviews.Locate(reviews.Lines(string(after)), c.Snippet, c.StartLine) == 0
}

func wireReviewComment(c reviews.Comment, a reviews.Anchor) *pb.ReviewComment {
	return &pb.ReviewComment{Id: c.ID, Member: c.Member, Path: c.Path, Side: c.Side, StartLine: uint32(c.StartLine), EndLine: uint32(c.EndLine), Snippet: c.Snippet, Hash: c.Hash, Body: c.Body,
		Head: c.Head, View: c.View, Base: c.Base, CreatedAtUnix: c.CreatedAt, UpdatedAtUnix: c.UpdatedAt, Disposition: string(c.Disposition),
		SentTo: c.SentTo, SentToTitle: c.SentToTitle, SentAtUnix: c.SentAt, SentHead: c.SentHead, ResolvedBy: c.ResolvedBy, Note: c.Note,
		Freshness: string(a.Freshness), CurrentStartLine: uint32(a.StartLine), CurrentEndLine: uint32(a.EndLine), FreshnessReason: a.Reason,
		Author: c.Author, AuthorLabel: c.AuthorLabel, AuthorSessionId: c.AuthorSession, Severity: c.Severity, Title: c.Title, ReviewId: c.ReviewID, FindingKey: c.Key,
		Round: uint32(c.Round), StillOpenRounds: uint32(c.StillOpenRounds)}
}

// wireReviewRun is a review as it stands now, with its live findings counted.
func (s *Service) wireReviewRun(review reviews.Review, run reviews.Run, workspace string) *pb.ReviewRun {
	run = s.Bench.ReviewRunView(run)
	counts := reviews.CountFindings(review, run.ID)
	out := &pb.ReviewRun{TemplateId: run.TemplateID, ResultArtifactId: run.ResultArtifact, Id: run.ID, CollectionId: workspace, ReviewerSessionId: run.ReviewerSession, ReviewerTitle: run.ReviewerTitle, ReviewerLabel: run.ReviewerLabel, ReviewerProvider: run.ReviewerProvider, ReviewerProfileId: run.ReviewerProfile,
		SourceSessionId: run.SourceSession, SourceTitle: run.SourceTitle, SourceLabel: run.SourceLabel, Scope: run.Scope, ScopeLabel: run.ScopeLabel, Turn: uint32(run.Turn), Compare: run.Compare, PullRequestUrl: run.PullRequestURL,
		State: run.State, Detail: run.Detail, CreatedAtUnix: run.CreatedAt, UpdatedAtUnix: run.UpdatedAt,
		Blockers: uint32(counts.Blockers), Majors: uint32(counts.Majors), Minors: uint32(counts.Minors), Nits: uint32(counts.Nits), MissionId: run.Mission, TaskId: run.Task, SourceProvider: run.SourceProvider,
		Loop: run.Loop, MaxRounds: uint32(run.MaxRounds), Round: uint32(run.Round), Phase: run.Phase, Paused: run.Paused, StopReason: run.StopReason, Fallback: run.Fallback, Focus: run.Focus, StoppedBy: run.StoppedBy, StoppedAtUnix: run.StoppedAt, Member: run.Member}
	if out.Loop == "" {
		// A review from before the loop: one pass.
		out.Loop, out.Round, out.MaxRounds = reviews.LoopOff, 1, 1
		if out.Phase = reviews.PhaseDone; run.State == reviews.RunRunning {
			out.Phase = reviews.PhaseReviewing
		}
	}
	for _, r := range run.Rounds {
		out.Rounds = append(out.Rounds, &pb.ReviewRound{Number: uint32(r.N), Trigger: r.Trigger, AuthorTurn: uint32(r.AuthorTurn), StartedAtUnix: r.StartedAt, EndedAtUnix: r.EndedAt,
			Filed: uint32(r.Filed), Resolved: uint32(r.Resolved), StillOpen: uint32(r.StillOpen), Blockers: uint32(r.Blockers)})
	}
	for _, e := range run.Timeline {
		out.Timeline = append(out.Timeline, &pb.ReviewTimelineEntry{AtUnix: e.At, Kind: e.Kind, Actor: e.Actor, Text: e.Text, FindingId: e.Finding, Round: uint32(e.Round)})
	}
	return out
}

// review anchors every comment against the working trees as they are now.
func (s *Service) review(ctx context.Context, store *reviews.Store, workspace string, review reviews.Review) *pb.WorkspaceReview {
	roots := s.memberRoots(workspace)
	out := &pb.WorkspaceReview{AutoResolve: review.AutoResolve}
	for i := len(review.Runs) - 1; i >= 0; i-- {
		out.Runs = append(out.Runs, s.wireReviewRun(review, review.Runs[i], workspace))
	}
	for _, c := range review.Comments {
		anchor := reviews.Anchor{Freshness: reviews.Unknown}
		root, err := roots.root(ctx, c.Member)
		if err != nil {
			anchor.Reason = "The repository is not available: " + err.Error()
		} else if content, exists, err := gitinspect.ReadWorking(root, c.Path); err != nil {
			anchor.Reason = "The file cannot be read: " + err.Error()
		} else {
			anchor = reviews.Evaluate(c, content, exists)
		}
		if review.AutoResolve && c.Disposition == reviews.Sent && err == nil {
			if head := roots.head(ctx, c.Member); committedAway(ctx, root, head, c, anchor) {
				if changed, _ := store.ResolveByCommit(workspace, c.ID, head); changed {
					if fresh, err := store.Load(workspace); err == nil {
						for _, r := range fresh.Comments {
							if r.ID == c.ID {
								c = r
							}
						}
					}
				}
			}
		}
		out.Comments = append(out.Comments, wireReviewComment(c, anchor))
	}
	return out
}

func (s *Service) ListReviewComments(ctx context.Context, r *pb.ListReviewCommentsRequest) (*pb.WorkspaceReview, error) {
	store, err := s.reviewStore(r.CollectionId)
	if err != nil {
		return nil, err
	}
	review, err := store.Load(r.CollectionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.review(ctx, store, r.CollectionId, review), nil
}

func (s *Service) AddReviewComment(ctx context.Context, r *pb.AddReviewCommentRequest) (*pb.WorkspaceReview, error) {
	store, err := s.reviewStore(r.CollectionId)
	if err != nil {
		return nil, err
	}
	roots := s.memberRoots(r.CollectionId)
	root, err := roots.root(ctx, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	fileHash := ""
	if content, exists, err := gitinspect.ReadWorking(root, r.Path); err == nil && exists {
		fileHash = reviews.FileHash(content)
	}
	added, err := store.Add(r.CollectionId, reviews.Comment{Member: r.Member, Path: r.Path, Side: r.Side, StartLine: int(r.StartLine), Snippet: r.Snippet, Body: r.Body,
		FileHash: fileHash, Head: roots.head(ctx, r.Member), View: r.View, Base: r.Base})
	if err != nil {
		return nil, rpcError(err)
	}
	review, err := store.Load(r.CollectionId)
	if err != nil {
		return nil, rpcError(err)
	}
	out := s.review(ctx, store, r.CollectionId, review)
	out.AddedId = added.ID
	return out, nil
}

func (s *Service) EditReviewComment(ctx context.Context, r *pb.EditReviewCommentRequest) (*pb.WorkspaceReview, error) {
	store, err := s.reviewStore(r.CollectionId)
	if err != nil {
		return nil, err
	}
	review, err := store.Edit(r.CollectionId, r.Id, r.Body)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.review(ctx, store, r.CollectionId, review), nil
}

func (s *Service) DeleteReviewComment(ctx context.Context, r *pb.DeleteReviewCommentRequest) (*pb.WorkspaceReview, error) {
	store, err := s.reviewStore(r.CollectionId)
	if err != nil {
		return nil, err
	}
	review, err := store.Delete(r.CollectionId, r.Id)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.review(ctx, store, r.CollectionId, review), nil
}

func (s *Service) SetReviewDisposition(ctx context.Context, r *pb.SetReviewDispositionRequest) (*pb.WorkspaceReview, error) {
	store, err := s.reviewStore(r.CollectionId)
	if err != nil {
		return nil, err
	}
	to := reviews.Handover{Session: r.SessionId, Title: r.SessionTitle, Heads: map[string]string{}}
	if reviews.Disposition(r.Disposition) == reviews.Sent {
		current, err := store.Load(r.CollectionId)
		if err != nil {
			return nil, rpcError(err)
		}
		roots, chosen := s.memberRoots(r.CollectionId), map[string]bool{}
		for _, id := range r.Ids {
			chosen[id] = true
		}
		for _, c := range current.Comments {
			if _, seen := to.Heads[c.Member]; chosen[c.ID] && !seen {
				to.Heads[c.Member] = roots.head(ctx, c.Member)
			}
		}
	}
	review, err := store.SetDisposition(r.CollectionId, r.Ids, reviews.Disposition(r.Disposition), to, r.Note)
	if err != nil {
		return nil, rpcError(err)
	}
	if reviews.Disposition(r.Disposition) == reviews.Sent {
		// Findings handed to the author: their review's loop waits for the fix.
		s.Bench.ReviewFindingsSent(r.CollectionId, r.Ids, r.SessionTitle)
		if review, err = store.Load(r.CollectionId); err != nil {
			return nil, rpcError(err)
		}
	}
	return s.review(ctx, store, r.CollectionId, review), nil
}

func (s *Service) SetReviewAutoResolve(ctx context.Context, r *pb.SetReviewAutoResolveRequest) (*pb.WorkspaceReview, error) {
	store, err := s.reviewStore(r.CollectionId)
	if err != nil {
		return nil, err
	}
	review, err := store.SetAutoResolve(r.CollectionId, r.Enabled)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.review(ctx, store, r.CollectionId, review), nil
}

// StartReview opens a read-only reviewer conversation on a scope of a
// conversation's work and sends it the brief (runner.StartReview). A pull
// request is read through the PR loop first, so the brief names it.
func (s *Service) StartReview(ctx context.Context, r *pb.StartReviewRequest) (*pb.StartReviewResponse, error) {
	if _, err := s.reviewStore(r.CollectionId); err != nil {
		return nil, err
	}
	req := runner.ReviewRequest{TemplateID: r.TemplateId, Workspace: r.CollectionId, Source: r.SourceSessionId, Scope: r.Scope, Compare: r.Compare, Turn: int(r.Turn), Member: r.Member, Profile: r.ProfileId, Provider: r.Provider, Focus: r.Focus, Model: r.Model}
	if r.Scope == reviews.ScopePR {
		pr, err := s.GetPullRequest(ctx, &pb.GetPullRequestRequest{CollectionId: r.CollectionId, Member: r.Member})
		if err != nil {
			return nil, err
		}
		if !pr.Found {
			return nil, grpcError(codes.FailedPrecondition, errors.New("this branch has no pull request to review"))
		}
		req.PullRequest = &runner.ReviewPullRequest{Number: pr.Number, URL: pr.Url, Title: pr.Title, Base: pr.Base, Head: pr.Branch}
	}
	e, run, err := s.Bench.StartReview(ctx, req)
	if err != nil && e == nil {
		return nil, rpcError(err)
	}
	// A review whose brief could not be sent is recorded as failed, and shown.
	review, _ := s.Bench.Reviews.Load(e.CollectionID)
	out := &pb.StartReviewResponse{Run: s.wireReviewRun(review, run, e.CollectionID)}
	if out.Session, err = s.sessionView(ctx, e); err != nil {
		return nil, err
	}
	return out, nil
}

// AddReviewFinding files a reviewer agent's finding (`lazybox review add`).
func (s *Service) AddReviewFinding(ctx context.Context, r *pb.AddReviewFindingRequest) (*pb.ReviewFinding, error) {
	c, updated, _, err := s.Bench.AddReviewFinding(ctx, r.SessionId, runner.FindingRequest{File: r.File, StartLine: int(r.StartLine), EndLine: int(r.EndLine), Severity: r.Severity, Title: r.Title, Body: r.Body, Key: r.Key})
	if err != nil {
		if errors.Is(err, reviews.ErrSeverity) {
			return nil, grpcError(codes.InvalidArgument, err)
		}
		if errors.Is(err, runner.ErrNotReviewer) {
			return nil, grpcError(codes.PermissionDenied, err)
		}
		return nil, rpcError(err)
	}
	return &pb.ReviewFinding{Finding: wireReviewComment(c, reviews.Anchor{Freshness: reviews.Current, StartLine: c.StartLine, EndLine: c.EndLine}), Updated: updated}, nil
}

// ListReviewFindings is the review an execution takes part in and its
// findings, anchored as they are now (`lazybox review list`).
func (s *Service) ListReviewFindings(ctx context.Context, r *pb.ListReviewFindingsRequest) (*pb.ReviewFindings, error) {
	run, findings, workspace, err := s.Bench.ReviewFindingsOf(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	review, err := s.Bench.Reviews.Load(workspace)
	if err != nil {
		return nil, rpcError(err)
	}
	all := s.review(ctx, s.Bench.Reviews, workspace, reviews.Review{AutoResolve: review.AutoResolve, Comments: findings})
	return &pb.ReviewFindings{Run: s.wireReviewRun(review, run, workspace), Findings: all.Comments}, nil
}

// SetReviewFindingState is the reviewer's resolve or reopen of one of its
// findings (`lazybox review resolve|reopen`).
func (s *Service) SetReviewFindingState(ctx context.Context, r *pb.SetReviewFindingStateRequest) (*pb.ReviewFinding, error) {
	c, err := s.Bench.ReviewVerdict(r.SessionId, r.FindingId, r.State, r.Note)
	if err != nil {
		if errors.Is(err, runner.ErrNotReviewer) {
			return nil, grpcError(codes.PermissionDenied, err)
		}
		return nil, rpcError(err)
	}
	return &pb.ReviewFinding{Finding: wireReviewComment(c, reviews.Anchor{Freshness: reviews.Unknown})}, nil
}

// UpdateReviewRun is the person's control of a review's loop.
func (s *Service) UpdateReviewRun(ctx context.Context, r *pb.UpdateReviewRunRequest) (*pb.WorkspaceReview, error) {
	store, err := s.reviewStore(r.CollectionId)
	if err != nil {
		return nil, err
	}
	if _, err = s.Bench.ControlReview(r.CollectionId, r.ReviewId, r.Action, r.Loop, int(r.MaxRounds)); err != nil {
		return nil, rpcError(err)
	}
	review, err := store.Load(r.CollectionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.review(ctx, store, r.CollectionId, review), nil
}

// CancelReview is the person's cancel of a review under way: the loop ends,
// the reviewer's turn is interrupted and its session ended, its findings kept.
func (s *Service) CancelReview(ctx context.Context, r *pb.CancelReviewRequest) (*pb.WorkspaceReview, error) {
	if r.ReviewId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("name the review to cancel"))
	}
	if r.CollectionId != "" {
		if _, err := s.reviewStore(r.CollectionId); err != nil {
			return nil, err
		}
	}
	if s.Bench.Reviews == nil {
		return nil, grpcError(codes.FailedPrecondition, errors.New("reviews are not available on this Runner"))
	}
	// A reviewer whose session would not end is an error, though the review
	// is cancelled already: no round starts after it either way.
	workspace, _, err := s.Bench.CancelReview(ctx, r.CollectionId, r.ReviewId, "You")
	if err != nil {
		if errors.Is(err, reviews.ErrReviewEnded) {
			return nil, grpcError(codes.FailedPrecondition, err)
		}
		return nil, rpcError(err)
	}
	review, err := s.Bench.Reviews.Load(workspace)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.review(ctx, s.Bench.Reviews, workspace, review), nil
}

func wireAsk(a reviews.Ask, review string) *pb.AgentAsk {
	return &pb.AgentAsk{Id: a.ID, ReviewId: review, State: a.State, Question: a.Question, Answer: a.Answer, AskerLabel: a.FromLabel, AnswererLabel: a.ToLabel,
		AskerSessionId: a.FromSession, AnswererSessionId: a.ToSession, CreatedAtUnix: a.CreatedAt, AnsweredAtUnix: a.AnsweredAt}
}

func askError(err error) error {
	switch {
	case errors.Is(err, reviews.ErrAskLimit):
		return grpcError(codes.ResourceExhausted, err)
	case errors.Is(err, reviews.ErrAskRefused):
		return grpcError(codes.FailedPrecondition, err)
	case errors.Is(err, runner.ErrNotInReview):
		return grpcError(codes.PermissionDenied, err)
	}
	return rpcError(err)
}

// AskAgent carries a question between the author and the reviewer of one
// review (`lazybox ask`).
func (s *Service) AskAgent(ctx context.Context, r *pb.AskAgentRequest) (*pb.AgentAsk, error) {
	a, err := s.Bench.AskAgent(ctx, runner.AskRequest{Session: r.SessionId, To: r.To, Question: r.Question, Wait: time.Duration(r.TimeoutSeconds) * time.Second})
	if err != nil && a.ID == "" {
		return nil, askError(err)
	}
	return wireAsk(a, ""), nil
}

// WaitAgentAsk waits for a question's reply; only its asker may.
func (s *Service) WaitAgentAsk(ctx context.Context, r *pb.WaitAgentAskRequest) (*pb.AgentAsk, error) {
	a, err := s.Bench.WaitAgentAsk(ctx, r.AskId, time.Duration(r.TimeoutSeconds)*time.Second)
	if err != nil && a.ID == "" {
		return nil, askError(err)
	}
	return wireAsk(a, ""), nil
}
