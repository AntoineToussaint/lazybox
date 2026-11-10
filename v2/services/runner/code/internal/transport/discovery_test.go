package transport

import (
	"context"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"path/filepath"
	"testing"
)

func TestRepositoryDiscoveryRPCPersistsWithoutRegistering(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	before := s.Bench.Collections.Repositories()
	empty, err := s.ListRepositoryDiscovery(ctx, &pb.ListRepositoryDiscoveryRequest{})
	if err != nil || len(empty.Roots) != 0 {
		t.Fatal(empty, err)
	}
	root, err := s.ScanRepositoryRoot(ctx, &pb.ScanRepositoryRootRequest{Path: filepath.Dir(before[0].Path)})
	if err != nil || len(root.Roots) != 1 || len(root.Roots[0].Repositories) == 0 {
		t.Fatal(root, err)
	}
	list, err := s.ListRepositoryDiscovery(ctx, &pb.ListRepositoryDiscoveryRequest{})
	if err != nil || len(list.Roots) != 1 {
		t.Fatal(list, err)
	}
	if len(s.Bench.Collections.Repositories()) != len(before) {
		t.Fatal("discovery registered a repository implicitly")
	}
}
