package transport

import (
	"context"
	"errors"
	"strconv"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// The PR loop. Every RPC resolves the member's worktree through the workspace
// manager, so a caller cannot name an arbitrary path, and acts on that one
// worktree and its branch. The authority interceptor admits only the owner.

func (s *Service) member(ctx context.Context, collectionID, name string) (string, string, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return "", "", err
	}
	path, err := manager.InspectionPath(ctx, collectionID, name)
	if err != nil {
		return "", "", err
	}
	workspace, err := manager.Get(collectionID)
	if err != nil {
		return "", "", err
	}
	for _, member := range workspace.Members {
		if member.Name == name {
			return path, member.BaseCommit, nil
		}
	}
	return "", "", errors.New("ready repository member not found")
}

func (s *Service) githubConnected() bool {
	return s.Bench.GitHub != nil && s.Bench.GitHub.Status().State == "connected"
}

const connectForPullRequests = "Connect GitHub in Settings → Repositories to open and track pull requests"

func (s *Service) CommitWorkspaceChanges(ctx context.Context, r *pb.CommitWorkspaceChangesRequest) (*pb.CommitWorkspaceChangesResponse, error) {
	path, _, err := s.member(ctx, r.CollectionId, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	result, err := gitpublish.Git{}.Commit(ctx, path, r.Message, r.Paths)
	if err != nil {
		return nil, rpcError(err)
	}
	s.Bench.RecordCommit(r.CollectionId, r.Member, result.Commit, result.Subject)
	return &pb.CommitWorkspaceChangesResponse{Commit: result.Commit, Subject: result.Subject, Files: uint32(result.Files), Branch: result.Branch}, nil
}

func (s *Service) PushWorkspaceBranch(ctx context.Context, r *pb.PushWorkspaceBranchRequest) (*pb.PushWorkspaceBranchResponse, error) {
	path, _, err := s.member(ctx, r.CollectionId, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	ctx, cancel := context.WithTimeout(ctx, 6*time.Minute)
	defer cancel()
	git := gitpublish.Git{}
	origin, err := git.Origin(ctx, path)
	if err != nil {
		return nil, rpcError(err)
	}
	var pushed gitpublish.Pushed
	if origin.HTTPS {
		if !s.githubConnected() {
			return nil, rpcError(errors.New("connect GitHub in Settings → Repositories to push to " + origin.GitHub))
		}
		err = s.Bench.GitHub.WithCredential(ctx, func(token string) error {
			pushed, err = git.Push(ctx, path, token)
			return err
		})
	} else {
		pushed, err = git.Push(ctx, path, "")
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.PushWorkspaceBranchResponse{Branch: pushed.Branch, Upstream: pushed.Upstream, Head: pushed.Head, GithubRepository: origin.GitHub}, nil
}

// pullRequestTarget is the member's GitHub repository and branch, or why the
// PR loop does not apply to it (returned as a status, not an error: it is a
// state of the repository, not a failure).
func (s *Service) pullRequestTarget(ctx context.Context, collectionID, name string) (path, base string, out *pb.PullRequestStatus, err error) {
	path, base, err = s.member(ctx, collectionID, name)
	if err != nil {
		return "", "", nil, err
	}
	git := gitpublish.Git{}
	out = &pb.PullRequestStatus{}
	if out.Branch, err = git.Branch(ctx, path); err != nil {
		out.Unavailable = err.Error()
		return path, base, out, nil
	}
	origin, err := git.Origin(ctx, path)
	switch {
	case err != nil:
		out.Unavailable = err.Error()
	case origin.GitHub == "":
		out.Unavailable = "origin is not a GitHub repository; push works, pull requests need GitHub"
	case !s.githubConnected() || s.Bench.GitHubPulls == nil:
		out.Unavailable = connectForPullRequests
	}
	out.GithubRepository = origin.GitHub
	return path, base, out, nil
}

func wirePullRequest(out *pb.PullRequestStatus, status github.PullRequestStatus) *pb.PullRequestStatus {
	out.DefaultBranch = status.DefaultBranch
	out.Found = status.Found
	out.Number = status.Number
	out.Title = status.Title
	out.Url = status.URL
	out.State = status.State
	out.Draft = status.Draft
	out.Base = status.Base
	out.HeadSha = status.HeadSHA
	out.MergeableState = status.MergeableState
	out.Mergeable = status.Mergeable
	out.ReviewDecision = status.ReviewDecision
	out.ChecksState = status.ChecksState
	out.MergeCommit = status.MergeCommit
	out.Author = status.Author
	out.Checks = nil
	for _, c := range status.Checks {
		out.Checks = append(out.Checks, &pb.PullRequestCheck{Name: c.Name, Status: c.Status, Conclusion: c.Conclusion, Url: c.URL, Summary: c.Summary})
	}
	return out
}

func (s *Service) readPullRequest(ctx context.Context, out *pb.PullRequestStatus) (*pb.PullRequestStatus, github.PullRequestStatus, error) {
	var status github.PullRequestStatus
	err := s.Bench.GitHub.WithCredential(ctx, func(token string) error {
		var err error
		status, err = s.Bench.GitHubPulls.PullRequestStatus(ctx, token, out.GithubRepository, out.Branch)
		if err == nil {
			wirePullRequest(out, status)
		}
		return err
	})
	if err != nil {
		return nil, status, rpcError(err)
	}
	return out, status, nil
}

func (s *Service) GetPullRequest(ctx context.Context, r *pb.GetPullRequestRequest) (*pb.PullRequestStatus, error) {
	_, _, out, err := s.pullRequestTarget(ctx, r.CollectionId, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	if out.Unavailable != "" {
		return out, nil
	}
	readAt := s.Bench.ItemReadStart()
	out, status, err := s.readPullRequest(ctx, out)
	if err == nil {
		s.observeLoop(r.CollectionId, r.Member, out, status, false, readAt)
	}
	return out, err
}

func (s *Service) CreatePullRequest(ctx context.Context, r *pb.CreatePullRequestRequest) (*pb.PullRequestStatus, error) {
	path, base, out, err := s.pullRequestTarget(ctx, r.CollectionId, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	if out.Unavailable != "" {
		return nil, rpcError(errors.New(out.Unavailable))
	}
	tracking, err := gitpublish.Git{}.Track(ctx, path, base)
	if err != nil {
		return nil, rpcError(err)
	}
	if tracking.Upstream == "" {
		return nil, rpcError(errors.New("push the branch before opening a pull request"))
	}
	if tracking.Ahead > 0 {
		return nil, rpcError(errors.New(strconv.Itoa(tracking.Ahead) + " local commit(s) are not pushed yet; push before opening a pull request"))
	}
	var status github.PullRequestStatus
	readAt := s.Bench.ItemReadStart()
	err = s.Bench.GitHub.WithCredential(ctx, func(token string) error {
		var err error
		status, err = s.Bench.GitHubPulls.CreatePullRequest(ctx, token, out.GithubRepository, github.NewPullRequest{Head: out.Branch, Base: r.Base, Title: r.Title, Body: r.Body, Draft: r.Draft})
		if err == nil {
			wirePullRequest(out, status)
		}
		return err
	})
	if err != nil {
		return nil, rpcError(err)
	}
	// The person opened it: the workspace delivers it (a tracked item).
	s.observeLoop(r.CollectionId, r.Member, out, status, true, readAt)
	return out, nil
}

func (s *Service) MergePullRequest(ctx context.Context, r *pb.MergePullRequestRequest) (*pb.PullRequestStatus, error) {
	if r.Item != nil && r.Item.Key != "" {
		return s.mergeTracked(ctx, r)
	}
	_, _, out, err := s.pullRequestTarget(ctx, r.CollectionId, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	if out.Unavailable != "" {
		return nil, rpcError(errors.New(out.Unavailable))
	}
	err = s.Bench.GitHub.WithCredential(ctx, func(token string) error {
		// The number must be this branch's pull request: a request cannot
		// merge some other pull request through this workspace.
		current, err := s.Bench.GitHubPulls.PullRequestStatus(ctx, token, out.GithubRepository, out.Branch)
		if err != nil {
			return err
		}
		if !current.Found || current.Number != r.Number {
			return errors.New("pull request #" + strconv.FormatInt(r.Number, 10) + " is not this branch's pull request")
		}
		return s.Bench.GitHubPulls.MergePullRequest(ctx, token, out.GithubRepository, r.Number, r.Method, r.ExpectedHead)
	})
	if err != nil {
		return nil, rpcError(err)
	}
	readAt := s.Bench.ItemReadStart()
	out, status, err := s.readPullRequest(ctx, out)
	if err == nil {
		s.observeLoop(r.CollectionId, r.Member, out, status, false, readAt)
	}
	return out, err
}

// UpdatePullRequestBranch brings a tracked pull request up to date with its
// base. The workspace must deliver it, and GitHub updates it only if its head
// is still the one the person saw.
func (s *Service) UpdatePullRequestBranch(ctx context.Context, r *pb.UpdatePullRequestBranchRequest) (*pb.PullRequestStatus, error) {
	if !s.githubConnected() {
		return nil, rpcError(errors.New(connectForPullRequests))
	}
	if r.Item == nil || r.Item.Key == "" {
		return nil, rpcError(errors.New("name the tracked pull request to update"))
	}
	ref := refOf(r.Item)
	status, _, err := s.Bench.UpdateTrackedPullRequestBranch(ctx, r.CollectionId, ref, r.Number, r.ExpectedHead)
	if err != nil {
		return nil, reactionError(err)
	}
	repository, _, _ := workitems.ParseGitHubKey(ref.Key)
	return wirePullRequest(&pb.PullRequestStatus{GithubRepository: repository, Branch: status.Head}, status), nil
}

// mergeTracked merges a pull request the workspace delivers on any branch
// (the Pull requests panel): the workspace must deliver it, and GitHub
// merges it only if its head is still the one the person saw.
func (s *Service) mergeTracked(ctx context.Context, r *pb.MergePullRequestRequest) (*pb.PullRequestStatus, error) {
	if !s.githubConnected() {
		return nil, rpcError(errors.New(connectForPullRequests))
	}
	ref := refOf(r.Item)
	status, _, err := s.Bench.MergeTrackedPullRequest(ctx, r.CollectionId, ref, r.Number, r.Method, r.ExpectedHead)
	if err != nil {
		return nil, reactionError(err)
	}
	repository, _, _ := workitems.ParseGitHubKey(ref.Key)
	return wirePullRequest(&pb.PullRequestStatus{GithubRepository: repository, Branch: status.Head}, status), nil
}
