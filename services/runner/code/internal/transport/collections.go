package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

func (s *Service) collectionStore() (*collections.Manager, error) {
	if s.Bench.Collections == nil {
		return nil, grpcError(codes.FailedPrecondition, errors.New("collections require a configured worktreeRoot"))
	}
	return s.Bench.Collections, nil
}
func wireCollection(c collections.Collection) *pb.Collection {
	w := &pb.Collection{Id: c.ID, Name: c.Name, WorkId: c.WorkID, Root: c.Root, State: c.State, Error: c.Error, OwnerExecutionId: c.OwnerExecutionID, OwnershipUncertain: c.OwnershipUncertain, OwnerExecutionIds: c.OwnerExecutionIDs, CreatedAtUnixMillis: c.CreatedAt}
	if c.Origin != nil {
		w.Origin = &pb.ItemRef{Provider: c.Origin.Provider, Key: c.Origin.Key}
	}
	for _, m := range c.Members {
		w.Members = append(w.Members, &pb.CollectionWorktree{Spec: &pb.WorktreeSpec{Name: m.Name, RepositoryId: m.RepositoryID, BaseRef: m.BaseRef, Branch: m.Branch}, Path: m.Path, BaseCommit: m.BaseCommit, Ready: m.Ready})
	}
	return w
}
func (s *Service) ListRepositories(ctx context.Context, r *pb.ListRepositoriesRequest) (*pb.ListRepositoriesResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	w := &pb.ListRepositoriesResponse{}
	for _, repo := range m.Repositories() {
		wire := wireRepository(repo, m.RepositoryIdentity(repo.ID))
		wire.HomePath, wire.HomeKind = m.Home(repo.ID)
		w.Repositories = append(w.Repositories, wire)
	}
	return w, nil
}
func (s *Service) CreateCollection(ctx context.Context, r *pb.CreateCollectionRequest) (*pb.CollectionResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	if r.Spec == nil {
		return nil, grpcError(codes.InvalidArgument, errors.New("collection specification required"))
	}
	spec := collections.Spec{Name: r.Spec.Name, WorkID: r.Spec.WorkId}
	for _, member := range r.Spec.Members {
		if member == nil {
			return nil, grpcError(codes.InvalidArgument, errors.New("worktree specification required"))
		}
		spec.Members = append(spec.Members, collections.MemberSpec{Name: member.Name, RepositoryID: member.RepositoryId, BaseRef: member.BaseRef, Branch: member.Branch})
	}
	s.Bench.RefreshBases(ctx, spec.Members)
	c, err := m.Create(ctx, r.RequestKey, spec)
	if err != nil {
		return nil, rpcError(err)
	}
	// The worktrees exist: the workspace runtime copies files and starts setup.
	_ = s.Bench.WorkspaceCreated(ctx, c.ID, nil, nil)
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}
func (s *Service) GetCollection(ctx context.Context, r *pb.CollectionRequest) (*pb.CollectionResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	c, err := m.Get(r.CollectionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}
func (s *Service) ListCollections(ctx context.Context, r *pb.ListCollectionsRequest) (*pb.ListCollectionsResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	w := &pb.ListCollectionsResponse{}
	hidden := s.hiddenWorkspaces()
	for _, c := range m.List() {
		if hidden[c.ID] {
			continue
		}
		w.Collections = append(w.Collections, wireCollection(c))
	}
	return w, nil
}

// ArrangeWorkspaces keeps the person's order of their workspaces: the one
// ListCollections answers in, in every window and on a paired phone.
func (s *Service) ArrangeWorkspaces(ctx context.Context, r *pb.ArrangeWorkspacesRequest) (*pb.ListCollectionsResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	list, err := m.Arrange(r.CollectionIds)
	if errors.Is(err, collections.ErrArrangementTooLong) {
		return nil, grpcError(codes.InvalidArgument, err)
	}
	if err != nil {
		return nil, rpcError(err)
	}
	w := &pb.ListCollectionsResponse{}
	hidden := s.hiddenWorkspaces()
	for _, c := range list {
		if hidden[c.ID] {
			continue
		}
		w.Collections = append(w.Collections, wireCollection(c))
	}
	return w, nil
}
func (s *Service) ArchiveCollection(ctx context.Context, r *pb.CollectionRequest) (*pb.CollectionResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	c, err := m.Archive(r.CollectionId, s.Bench.Active)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}

func (s *Service) ReconcileCollection(ctx context.Context, r *pb.ReconcileCollectionRequest) (*pb.CollectionResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	c, err := m.Reconcile(r.CollectionId, r.ExpectedOwnerExecutionId, r.Reason, r.ProcessesStopped)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}

func wireRepository(repo collections.Repository, identity string) *pb.Repository {
	return &pb.Repository{Id: repo.ID, Path: repo.Path, Name: repo.Name, SourceUrl: repo.SourceURL, GithubId: repo.GitHubID, GitIdentity: identity}
}

func (s *Service) AddLocalRepository(ctx context.Context, r *pb.AddLocalRepositoryRequest) (*pb.AddLocalRepositoryResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	repo, err := m.AddLocalRepository(ctx, r.Path, r.Name)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.AddLocalRepositoryResponse{Repository: wireRepository(repo, m.RepositoryIdentity(repo.ID))}, nil
}

func (s *Service) AddCollectionRepositories(ctx context.Context, r *pb.AddCollectionRepositoriesRequest) (*pb.CollectionResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	members := []collections.MemberSpec{}
	for _, member := range r.Members {
		if member == nil {
			return nil, grpcError(codes.InvalidArgument, errors.New("repository specification required"))
		}
		members = append(members, collections.MemberSpec{Name: member.Name, RepositoryID: member.RepositoryId, BaseRef: member.BaseRef, Branch: member.Branch})
	}
	before := map[string]bool{}
	if existing, err := m.Get(r.CollectionId); err == nil {
		for _, member := range existing.Members {
			before[member.Name] = true
		}
	}
	s.Bench.RefreshBases(ctx, members)
	c, err := m.AddRepositories(ctx, r.CollectionId, r.RequestKey, members)
	if err != nil {
		return nil, rpcError(err)
	}
	added := []string{}
	for _, member := range c.Members {
		if !before[member.Name] {
			added = append(added, member.Name)
		}
	}
	if len(added) > 0 {
		_ = s.Bench.WorkspaceCreated(ctx, c.ID, added, nil)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}

func (s *Service) RemoveCollectionRepository(ctx context.Context, r *pb.RemoveCollectionRepositoryRequest) (*pb.CollectionResponse, error) {
	c, err := s.Bench.RemoveWorkspaceRepository(ctx, r.CollectionId, r.MemberName)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}
