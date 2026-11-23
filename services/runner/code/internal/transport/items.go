package transport

import (
	"context"
	"errors"
	"strconv"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Tracked work items (runner/items.go). Every read is the store's: nothing
// here waits on GitHub, except the reactions, which read logs and comments.

const linkEventsListed = 50

func (s *Service) itemStore() (*workitems.Store, error) {
	if s.Bench == nil || s.Bench.WorkItems() == nil {
		return nil, grpcError(codes.Unavailable, errors.New("item tracking is unavailable on this Runner"))
	}
	return s.Bench.WorkItems(), nil
}

// links is one workspace's links (or every workspace's), wired.
func (s *Service) links(workspace string, all bool) (*pb.Links, error) {
	store, err := s.itemStore()
	if err != nil {
		return nil, err
	}
	filter := workitems.Filter{Workspace: workspace}
	if all {
		filter = workitems.Filter{}
	}
	out := &pb.Links{}
	// A workspace started from an issue lists it first, as its origin. It is
	// read from the workspace's record (collections.Origin), not tracked.
	if !all && s.Bench.Collections != nil {
		if c, err := s.Bench.Collections.Get(workspace); err == nil && c.Origin != nil {
			o := c.Origin
			link := &pb.ItemLink{CollectionId: workspace, Role: "origin", Source: "started-from",
				Item: &pb.Item{Ref: &pb.ItemRef{Provider: o.Provider, Key: o.Key}, Kind: o.Kind, Title: o.Title, Url: o.URL, ClosingReference: o.ClosingReference}}
			if !o.StartedAt.IsZero() {
				link.LinkedAtUnixMillis = o.StartedAt.UnixMilli()
			}
			out.Links = append(out.Links, link)
		}
	}
	for _, l := range store.Links(filter) {
		out.Links = append(out.Links, wireLink(l))
	}
	scope := workspace
	if all {
		scope = ""
	}
	for _, ev := range store.Events(scope, linkEventsListed) {
		out.Events = append(out.Events, wireItemEvent(ev))
	}
	every, reason := s.Bench.ItemTrackingStatus(scope)
	out.PollSeconds, out.TrackingError = uint32(every.Seconds()), reason
	return out, nil
}

func (s *Service) ListLinks(ctx context.Context, r *pb.ListLinksRequest) (*pb.Links, error) {
	workspace := r.CollectionId
	if r.SessionId != "" {
		e, err := s.Bench.Get(r.SessionId)
		if err != nil {
			return nil, rpcError(err)
		}
		workspace = e.CollectionID
		if workspace == "" {
			return &pb.Links{}, nil
		}
	}
	if !r.All && workspace == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("a workspace, a session or all is required"))
	}
	if r.Focused {
		s.Bench.ItemsFocused()
	}
	return s.links(workspace, r.All)
}

func (s *Service) LinkItem(ctx context.Context, r *pb.LinkItemRequest) (*pb.Links, error) {
	if _, err := s.itemStore(); err != nil {
		return nil, err
	}
	// An agent's link (its own credential) is bounded and rate limited, and
	// delivers only a pull request of a workspace repository's origin.
	source := workitems.SourcePerson
	if callerSession(ctx) != "" {
		source = workitems.SourceCLI
	}
	if _, err := s.Bench.LinkItemURL(r.SessionId, r.CollectionId, r.Url, r.Role, source); err != nil {
		if errors.Is(err, runner.ErrLinkRate) || errors.Is(err, workitems.ErrTooManyDelivered) {
			return nil, grpcError(codes.ResourceExhausted, err)
		}
		return nil, grpcError(codes.InvalidArgument, err)
	}
	workspace := r.CollectionId
	if r.SessionId != "" {
		if e, err := s.Bench.Get(r.SessionId); err == nil {
			workspace = e.CollectionID
		}
	}
	return s.links(workspace, false)
}

func (s *Service) UnlinkItem(ctx context.Context, r *pb.UnlinkItemRequest) (*pb.Links, error) {
	store, err := s.itemStore()
	if err != nil {
		return nil, err
	}
	if r.CollectionId == "" || r.Ref == nil || r.Ref.Key == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("a workspace and an item are required"))
	}
	if err = store.Unlink(r.CollectionId, refOf(r.Ref)); err != nil {
		return nil, rpcError(err)
	}
	return s.links(r.CollectionId, false)
}

func refOf(r *pb.ItemRef) workitems.Ref {
	if r == nil {
		return workitems.Ref{}
	}
	provider := r.Provider
	if provider == "" {
		provider = "github"
	}
	return workitems.Ref{Provider: provider, Key: r.Key}
}

func (s *Service) SendPullRequestFailures(ctx context.Context, r *pb.PullRequestReactionRequest) (*pb.PullRequestReaction, error) {
	if r.CollectionId == "" || r.Ref == nil {
		return nil, grpcError(codes.InvalidArgument, errors.New("a workspace and a pull request are required"))
	}
	out, err := s.Bench.SendPullRequestFailures(ctx, r.CollectionId, refOf(r.Ref), r.SessionId, r.ExpectedObservation)
	if err != nil {
		return nil, reactionError(err)
	}
	return wireReaction(out), nil
}

func (s *Service) ImportPullRequestReview(ctx context.Context, r *pb.PullRequestReactionRequest) (*pb.PullRequestReaction, error) {
	if r.CollectionId == "" || r.Ref == nil {
		return nil, grpcError(codes.InvalidArgument, errors.New("a workspace and a pull request are required"))
	}
	out, err := s.Bench.ImportPullRequestReview(ctx, r.CollectionId, refOf(r.Ref), r.SessionId, r.ExpectedObservation)
	if err != nil {
		return nil, reactionError(err)
	}
	return wireReaction(out), nil
}

func reactionError(err error) error {
	if errors.Is(err, runner.ErrNotDelivered) {
		return grpcError(codes.NotFound, err)
	}
	return rpcError(err)
}

func wireReaction(r runner.Reaction) *pb.PullRequestReaction {
	return &pb.PullRequestReaction{SessionId: r.Session, Queued: r.Queued, Summary: r.Summary, Files: r.Files, Notes: r.Notes, Comments: uint32(r.Comments)}
}

func wireLink(l workitems.LinkView) *pb.ItemLink {
	return &pb.ItemLink{Item: wireItem(l.Item), CollectionId: l.Workspace, SessionId: l.Session, ConversationId: l.Conversation, Role: l.Role, Source: l.Source,
		LinkedAtUnixMillis: millis(l.At), CollectionName: l.WorkspaceName, CollectionDeleted: l.Deleted, Member: l.Member}
}

func wireItem(item workitems.Item) *pb.Item {
	out := &pb.Item{Ref: &pb.ItemRef{Provider: item.Ref.Provider, Key: item.Ref.Key}, Kind: item.Kind, Title: item.Title, Url: item.URL, Repository: item.Repository, Author: item.Author,
		UpdatedAtUnixMillis: millis(item.UpdatedAt), ObservedAtUnixMillis: millis(item.ObservedAt)}
	if item.State != "" {
		out.State = &pb.ItemState{Category: item.State, Label: item.Label}
	}
	if out.Url == "" {
		if repository, number, ok := workitems.ParseGitHubKey(item.Ref.Key); ok && item.Ref.Provider == "github" {
			out.Url = "https://github.com/" + repository + "/pull/" + itoa(number)
			if out.Repository == "" {
				out.Repository = repository
			}
		}
	}
	if pr := item.PR; pr != nil {
		w := &pb.ItemPullRequest{ObservationId: item.ObservationID(), Head: pr.Head, Base: pr.Base, Draft: pr.Draft, Checks: pr.Checks, Review: pr.Review, Mergeable: pr.Mergeable, HeadSha: pr.HeadSHA,
			MergeableState: pr.MergeableState, ChecksTotal: uint32(len(pr.CheckRuns)), ClosedAtUnixMillis: millis(item.ClosedAt)}
		for _, c := range pr.Failing() {
			w.FailingChecks = append(w.FailingChecks, &pb.ItemCheck{Name: c.Name, Url: c.URL})
		}
		out.PullRequest = w
	}
	return out
}

func wireItemEvent(ev workitems.Event) *pb.ItemEvent {
	return &pb.ItemEvent{Id: ev.ID, Name: ev.Name, Ref: &pb.ItemRef{Provider: ev.Ref.Provider, Key: ev.Ref.Key}, CollectionId: ev.Workspace, SessionId: ev.Session,
		AtUnixMillis: millis(ev.At), Reason: ev.Reason, HeadSha: ev.HeadSHA, Title: ev.Title, Url: ev.URL}
}

// pullSummary is the sidebar's view of a tracked pull request.
func pullSummary(item workitems.Item) *pb.PullRequestSummary {
	out := &pb.PullRequestSummary{Number: item.Number, Title: item.Title, Url: item.URL, State: item.RawState, ObservedAtUnixMillis: millis(item.ObservedAt)}
	if item.PR != nil {
		out.Draft, out.ChecksState, out.ReviewDecision = item.PR.Draft, item.PR.Checks, item.PR.Review
	}
	return out
}

// statusOf is what the PR loop read, as the GitHub source reports it.
func statusOf(out *pb.PullRequestStatus) github.PullRequestStatus {
	status := github.PullRequestStatus{DefaultBranch: out.DefaultBranch, Found: out.Found, Number: out.Number, Title: out.Title, URL: out.Url, State: out.State, Draft: out.Draft,
		Base: out.Base, HeadSHA: out.HeadSha, Head: out.Branch, MergeableState: out.MergeableState, Mergeable: out.Mergeable, ReviewDecision: out.ReviewDecision,
		ChecksState: out.ChecksState, MergeCommit: out.MergeCommit, Author: out.Author}
	for _, c := range out.Checks {
		status.Checks = append(status.Checks, github.Check{Name: c.Name, Status: c.Status, Conclusion: c.Conclusion, URL: c.Url, Summary: c.Summary, Source: "run"})
	}
	return status
}

func itoa(n int64) string { return strconv.FormatInt(n, 10) }
