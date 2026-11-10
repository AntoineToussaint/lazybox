package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/repositories"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

func wireDiscovery(c repositories.Catalog) *pb.RepositoryDiscovery {
	out := &pb.RepositoryDiscovery{}
	for _, root := range c.Roots {
		item := &pb.RepositorySearchRoot{Path: root.Path, ScannedAt: root.ScannedAt, Truncated: root.Truncated, Skipped: uint32(root.Skipped)}
		for _, repo := range root.Candidates {
			item.Repositories = append(item.Repositories, &pb.DiscoveredRepository{Path: repo.Path, Name: repo.Name, GitIdentity: repo.GitIdentity})
		}
		out.Roots = append(out.Roots, item)
	}
	return out
}
func (s *Service) ListRepositoryDiscovery(context.Context, *pb.ListRepositoryDiscoveryRequest) (*pb.RepositoryDiscovery, error) {
	return wireDiscovery(s.Bench.RepositoryDiscovery.List()), nil
}
func (s *Service) ScanRepositoryRoot(ctx context.Context, r *pb.ScanRepositoryRootRequest) (*pb.RepositoryDiscovery, error) {
	catalog, err := s.Bench.RepositoryDiscovery.Scan(ctx, r.Path)
	return wireDiscovery(catalog), rpcError(err)
}

func (s *Service) RemoveRepository(ctx context.Context, r *pb.RemoveRepositoryRequest) (*pb.RepositoryDiscovery, error) {
	identity := r.GitIdentity
	if r.RepositoryId != "" {
		m, err := s.collectionStore()
		if err != nil {
			return nil, err
		}
		identity = m.RepositoryIdentity(r.RepositoryId)
		if err = m.RemoveRepository(r.RepositoryId); err != nil {
			return nil, rpcError(err)
		}
	}
	if s.Bench.RepositoryDiscovery == nil {
		return nil, rpcError(errors.New("repository discovery unavailable"))
	}
	if err := s.Bench.RepositoryCatalog.Remove(identity); err != nil {
		return nil, rpcError(err)
	}
	catalog, err := s.Bench.RepositoryDiscovery.Remove(identity)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireDiscovery(catalog), nil
}
