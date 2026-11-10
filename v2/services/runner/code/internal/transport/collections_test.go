package transport

import (
	"connectrpc.com/connect"
	"connectrpc.com/vanguard/vanguardgrpc"
	"context"
	"net"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"github.com/AntoineToussaint/lazybox/v2/pkg/gen/genconnect"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
)

func TestRepositoryWireRetainsLogicalIdentity(t *testing.T) {
	wire := wireRepository(collections.Repository{ID: "legacy-id", Name: "api", Path: "/private/source", SourceURL: "https://github.com/team/api"}, "github:team/api")
	if wire.GitIdentity != "github:team/api" || wire.SourceUrl != "https://github.com/team/api" || wire.Id != "legacy-id" {
		t.Fatal("repository identity lost at RPC boundary", wire)
	}
}

func collectionService(t *testing.T) (*Service, *agenttest.Adapter) {
	t.Helper()
	t.Setenv("CLAUDE_CONFIG_DIR", t.TempDir())
	path := t.TempDir()
	for _, args := range [][]string{{"init", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		cmd := exec.Command("git", append([]string{"-C", path}, args...)...)
		if data, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("git fixture: %v %s", err, data)
		}
	}
	b, err := runner.New(runner.Config{State: t.TempDir(), WorktreeRoot: t.TempDir(), Repositories: []collections.Repository{{ID: "api", Path: path}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	a := &agenttest.Adapter{}
	b.Adapters["claude"] = a
	return &Service{Bench: b}, a
}
func createTestCollection(t *testing.T, s *Service) *pb.Collection {
	t.Helper()
	r, err := s.CreateCollection(context.Background(), &pb.CreateCollectionRequest{RequestKey: "feature", Spec: &pb.CollectionSpec{Name: "Feature", WorkId: "tracker-record", Members: []*pb.WorktreeSpec{{Name: "api", RepositoryId: "api"}, {Name: "api-next", RepositoryId: "api"}}}})
	if err != nil {
		t.Fatal(err)
	}
	return r.Collection
}

func TestCollectionRPCsAttachAndContinueWithStableEnvironment(t *testing.T) {
	s, a := collectionService(t)
	ctx := context.Background()
	repos, err := s.ListRepositories(ctx, &pb.ListRepositoriesRequest{})
	if err != nil || len(repos.Repositories) != 1 {
		t.Fatal("repository discovery failed", err)
	}
	c := createTestCollection(t, s)
	list, err := s.ListCollections(ctx, &pb.ListCollectionsRequest{})
	if err != nil || len(list.Collections) != 1 {
		t.Fatal("collection discovery failed", err)
	}
	get, err := s.GetCollection(ctx, &pb.CollectionRequest{CollectionId: c.Id})
	if err != nil || get.Collection.Root != c.Root || len(get.Collection.Members) != 2 {
		t.Fatal("collection inspection failed", err)
	}
	if _, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", Cwd: c.Root, CollectionId: c.Id}); err == nil {
		t.Fatal("client path overrode collection")
	}
	if _, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", Cwd: filepath.Join(c.Root, "api")}); err == nil {
		t.Fatal("raw cwd bypassed collection attachment")
	}
	e, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: c.Id})
	if err != nil {
		t.Fatal(err)
	}
	if e.Session.CollectionId != c.Id || e.Session.WorkId != c.WorkId || e.Session.WorkingDirectory != c.Root || a.Session.Open.Cwd != c.Root {
		t.Fatal("execution not attached to collection")
	}
	for _, member := range c.Members {
		if !strings.Contains(a.Session.Open.Context, member.Path) || !strings.Contains(a.Session.Open.Context, member.BaseCommit) {
			t.Fatal("agent missing repository context")
		}
	}
	second, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: c.Id})
	if err != nil || second.Session.Id == e.Session.Id || second.Session.WorkingDirectory != c.Root {
		t.Fatal("independent session in shared workspace", err)
	}
	if _, err = s.CloseSession(ctx, &pb.SessionRequest{SessionId: second.Session.Id}); err != nil {
		t.Fatal(err)
	}
	if _, err := s.ArchiveCollection(ctx, &pb.CollectionRequest{CollectionId: c.Id}); err == nil {
		t.Fatal("active collection archived")
	}
	next, err := s.ContinueSession(ctx, &pb.ContinueSessionRequest{SessionId: e.Session.Id, Mode: "native", Reason: "context continuation"})
	if err != nil || next.Session.CollectionId != c.Id || next.Session.WorkingDirectory != c.Root || next.Session.WorkId != e.Session.WorkId || next.Session.PreviousExecutionId != e.Session.Id {
		t.Fatal("continuation lost collection", err)
	}
	if _, err := s.CloseSession(ctx, &pb.SessionRequest{SessionId: next.Session.Id}); err != nil {
		t.Fatal(err)
	}
	if _, err := s.ArchiveCollection(ctx, &pb.CollectionRequest{CollectionId: c.Id}); err != nil {
		t.Fatal(err)
	}
	if _, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: c.Id}); err == nil {
		t.Fatal("archived collection accepted execution")
	}
}

func TestCollectionLeaseSurvivesClosingAgentWithRunningCommand(t *testing.T) {
	s, _ := collectionService(t)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	c := createTestCollection(t, s)
	e, err := s.Bench.OpenCollection("claude-default", c.Id, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	command, err := s.Bench.Start(e, "fixture", []string{"sh", "-c", "printf '%s' \"$PWD\"; exec sleep 30"})
	if err != nil {
		t.Fatal(err)
	}
	if !s.Bench.Active(e.ID) {
		t.Fatal("live execution reported inactive")
	}
	if _, err := s.CloseSession(ctx, &pb.SessionRequest{SessionId: e.ID}); err != nil {
		t.Fatal(err)
	}
	sibling, err := s.Bench.OpenCollection("claude-default", c.Id, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal("sibling session rejected", err)
	}
	if _, err = s.CloseSession(ctx, &pb.SessionRequest{SessionId: sibling.ID}); err != nil {
		t.Fatal(err)
	}
	if _, err := s.ArchiveCollection(ctx, &pb.CollectionRequest{CollectionId: c.Id}); err == nil {
		t.Fatal("archived collection with live command")
	}
	if err := command.Cancel(); err != nil {
		t.Fatal(err)
	}
	if _, err := command.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	if s.Bench.Active(e.ID) {
		t.Fatal("finished execution retains ownership forever")
	}
	if _, err := s.Bench.OpenCollection("claude-default", c.Id, "", accounts.Options{}, "direct", ""); err != nil {
		t.Fatal("collection not reusable after command completion", err)
	}
	if _, err := s.Bench.Start(e, "too late", []string{"true"}); err == nil {
		t.Fatal("closed execution started command")
	}
	if _, err := s.Bench.RegisterPipe(e, "too late"); err == nil {
		t.Fatal("closed execution registered pipe")
	}
}

func TestPipeAlsoRetainsCollectionUntilCaptureCompletes(t *testing.T) {
	s, _ := collectionService(t)
	c := createTestCollection(t, s)
	ctx := context.Background()
	e, err := s.Bench.OpenCollection("claude-default", c.Id, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	pipe, err := s.RegisterPipe(ctx, &pb.RegisterPipeRequest{SessionId: e.ID, Title: "fixture"})
	if err != nil {
		t.Fatal(err)
	}
	e.Engine.Close()
	if !s.Bench.Active(e.ID) {
		t.Fatal("live output producer lost ownership")
	}
	if _, err := s.FinishPipe(ctx, &pb.FinishPipeRequest{CommandId: pipe.Id}); err != nil {
		t.Fatal(err)
	}
	if _, err := s.ArchiveCollection(ctx, &pb.CollectionRequest{CollectionId: c.Id}); err != nil {
		t.Fatal(err)
	}
}

func TestFailedCollectionSessionStartupReleasesReservation(t *testing.T) {
	s, _ := collectionService(t)
	c := createTestCollection(t, s)
	if _, err := s.Bench.OpenCollection("unknown-profile", c.Id, "", accounts.Options{}, "direct", ""); err == nil {
		t.Fatal("unknown profile accepted")
	}
	if _, err := s.Bench.OpenCollection("claude-default", c.Id, "", accounts.Options{}, "direct", ""); err != nil {
		t.Fatal("failed startup leaked reservation", err)
	}
	if _, err := s.CreateCollection(context.Background(), &pb.CreateCollectionRequest{}); err == nil {
		t.Fatal("missing spec accepted")
	}
	if _, err := s.GetCollection(context.Background(), &pb.CollectionRequest{CollectionId: "missing"}); err == nil {
		t.Fatal("missing collection accepted")
	}
}

type blockingCollectionAdapter struct{ entered chan struct{} }

func (a blockingCollectionAdapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	close(a.entered)
	<-ctx.Done()
	return nil, ctx.Err()
}

func TestShutdownCancelsOpeningAgentBeforeReleasingWorktreeRoot(t *testing.T) {
	s, _ := collectionService(t)
	c := createTestCollection(t, s)
	a := blockingCollectionAdapter{entered: make(chan struct{})}
	s.Bench.Adapters["claude"] = a
	opened := make(chan error, 1)
	go func() {
		_, err := s.Bench.OpenCollection("claude-default", c.Id, "", accounts.Options{}, "direct", "")
		opened <- err
	}()
	<-a.entered
	var wg sync.WaitGroup
	for range 2 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if err := s.Bench.Close(); err != nil {
				t.Error(err)
			}
		}()
	}
	wg.Wait()
	if err := <-opened; err == nil {
		t.Fatal("opening agent survived shutdown")
	}
	if _, err := s.Bench.OpenCollection("claude-default", c.Id, "", accounts.Options{}, "direct", ""); err == nil {
		t.Fatal("closed Runner accepted attachment")
	}
	m, err := collections.New(s.Bench.Config.WorktreeRoot, s.Bench.Config.Repositories)
	if err != nil {
		t.Fatal("root lock not released after shutdown", err)
	}
	m.Close()
	if _, err := os.Stat(filepath.Join(c.Root, "api", ".git")); err != nil {
		t.Fatal("shutdown deleted worktree")
	}
}

func TestCollectionGeneratedClientsShareNativeGRPCImplementation(t *testing.T) {
	s, _ := collectionService(t)
	server := grpc.NewServer(LocalAuthority("collection-fixture")...)
	pb.RegisterRunnerServiceServer(server, s)
	listener := bufconn.Listen(1 << 20)
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { server.Stop(); listener.Close() })
	conn, err := grpc.NewClient("passthrough:///collection-fixture", grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithContextDialer(func(ctx context.Context, target string) (net.Conn, error) { return listener.DialContext(ctx) }))
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	native := pb.NewRunnerServiceClient(conn)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if _, err := native.ListCollections(ctx, &pb.ListCollectionsRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatal("collection API bypassed authority", err)
	}
	// Same in-process transcoding mechanism used by Codefly's generated
	// Connect listener. This is a contract test, not a Codefly startup test.
	transcoder, err := vanguardgrpc.NewTranscoder(server)
	if err != nil {
		t.Fatal(err)
	}
	host := httptest.NewServer(transcoder)
	defer host.Close()
	client := genconnect.NewRunnerServiceClient(host.Client(), host.URL)
	request := connect.NewRequest(&pb.CreateCollectionRequest{RequestKey: "wire", Spec: &pb.CollectionSpec{Name: "Wire contract", Members: []*pb.WorktreeSpec{{Name: "api", RepositoryId: "api"}}}})
	request.Header().Set("Authorization", "Bearer collection-fixture")
	created, err := client.CreateCollection(ctx, request)
	if err != nil {
		t.Fatal(err)
	}
	authorized := metadata.NewOutgoingContext(ctx, metadata.Pairs("authorization", "Bearer collection-fixture"))
	read, err := native.GetCollection(authorized, &pb.CollectionRequest{CollectionId: created.Msg.Collection.Id})
	if err != nil || read.Collection.Root != created.Msg.Collection.Root || len(read.Collection.Members) != 1 {
		t.Fatal("Connect and native gRPC disagree", err)
	}
	e, err := native.OpenSession(authorized, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: read.Collection.Id})
	if err != nil || e.Session.CollectionId != read.Collection.Id || e.Session.WorkingDirectory != read.Collection.Root {
		t.Fatal("generated session contract lost collection", err)
	}
}

func TestReconcileCollectionRPCRequiresSpecificUncertainOwner(t *testing.T) {
	s, _ := collectionService(t)
	c := createTestCollection(t, s)
	ctx := context.Background()
	if _, err := s.Bench.Collections.Acquire(ctx, c.Id, "lost-registry-fixture", func(string) bool { return false }); err != nil {
		t.Fatal(err)
	}
	s.Bench.Collections.Close()
	m, err := collections.New(s.Bench.Config.WorktreeRoot, s.Bench.Config.Repositories)
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.Collections = m
	request := &pb.ReconcileCollectionRequest{CollectionId: c.Id, ExpectedOwnerExecutionId: "lost-registry-fixture", Reason: "storage fixture has no actual agent processes"}
	if _, err := s.ReconcileCollection(ctx, request); err == nil {
		t.Fatal("missing attestation accepted")
	}
	request.ProcessesStopped = true
	resolved, err := s.ReconcileCollection(ctx, request)
	if err != nil || resolved.Collection.OwnershipUncertain || resolved.Collection.OwnerExecutionId != "" {
		t.Fatal("ownership not reconciled", err)
	}
}

func TestLocalRepositoryRPCWorksWithNoGitHubConnection(t *testing.T) {
	s, adapter := collectionService(t)
	ctx := context.Background()
	if state := s.Bench.GitHub.Status().State; state != "setup_required" {
		t.Fatal(state)
	}
	path := t.TempDir()
	for _, args := range [][]string{{"init", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		cmd := exec.Command("git", append([]string{"-C", path}, args...)...)
		if data, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("fixture: %s %v", data, err)
		}
	}
	added, err := s.AddLocalRepository(ctx, &pb.AddLocalRepositoryRequest{Path: path, Name: "local-api"})
	if err != nil || added.Repository.Name != "local-api" {
		t.Fatal(added, err)
	}
	retry, err := s.AddLocalRepository(ctx, &pb.AddLocalRepositoryRequest{Path: path, Name: "local-api"})
	if err != nil || retry.Repository.Id != added.Repository.Id {
		t.Fatal("retry duplicated entry", err)
	}
	workspace, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: "local-workspace", Spec: &pb.CollectionSpec{Name: "local-feature", Members: []*pb.WorktreeSpec{{RepositoryId: added.Repository.Id}}}})
	if err != nil {
		t.Fatal(err)
	}
	session, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: workspace.Collection.Id})
	if err != nil || session.Session.WorkingDirectory != workspace.Collection.Root || adapter.Session.Open.Cwd != workspace.Collection.Root {
		t.Fatal(session, err)
	}
	if s.Bench.GitHub.Status().State != "setup_required" {
		t.Fatal("local setup changed GitHub")
	}
}
