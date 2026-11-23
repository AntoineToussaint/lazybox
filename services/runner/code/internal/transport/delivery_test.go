package transport

import (
	"context"
	"net"
	"os"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
	"google.golang.org/protobuf/proto"
)

const catalogSpec = `{"schema":"spec.v2","problem":"Deliver the work catalog","requirements":["Persist work items"],"acceptanceCriteria":[` +
	`{"id":"storage","expected":"Duplicate inserts keep one item","required":true},` +
	`{"id":"generate","expected":"The catalog service is generated","required":true},` +
	`{"id":"api","expected":"An authorized client reads the item","required":true}]}`

// serve puts b behind the real authority on an in-memory listener.
func serve(t *testing.T, b *runner.Bench) pb.RunnerServiceClient {
	t.Helper()
	server := grpc.NewServer(LocalAuthority(ownerToken, b)...)
	pb.RegisterRunnerServiceServer(server, &Service{Bench: b})
	listener := bufconn.Listen(1 << 20)
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { server.Stop(); listener.Close() })
	conn, err := grpc.NewClient("passthrough:///delivery", grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return listener.DialContext(ctx) }))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	return pb.NewRunnerServiceClient(conn)
}

// check runs one `lazybox run --check|--needs` the way the CLI does: register
// the pipe with the agent's credential, then finish it with its exit status.
func check(t *testing.T, ctx context.Context, client pb.RunnerServiceClient, session, criterion string, needs bool, exit int32) *pb.CommandStatus {
	t.Helper()
	r := &pb.RegisterPipeRequest{SessionId: session, Title: criterion, Check: criterion}
	if needs {
		r.Check, r.Needs = "", criterion
	}
	pipe, err := client.RegisterPipe(ctx, r)
	if err != nil {
		t.Fatal(err)
	}
	if pipe.Check != criterion {
		t.Fatalf("the Runner did not confirm the check: %+v", pipe)
	}
	path := t.TempDir() + "/output.log"
	if err := os.WriteFile(path, []byte("output of "+criterion), 0o600); err != nil {
		t.Fatal(err)
	}
	done, err := client.FinishPipe(ctx, &pb.FinishPipeRequest{CommandId: pipe.Id, Report: &pb.CommandReport{ExitCode: exit, OutputPath: path}})
	if err != nil {
		t.Fatal(err)
	}
	return done
}

func states(c *pb.DeliveryCandidateOutcome) map[string]string {
	out := map[string]string{}
	for _, criterion := range c.Criteria {
		out[criterion.Id] = criterion.State
	}
	return out
}

// Release qualification without a Mission (roadmap milestone G): the catalog
// failure, partial repair and success through the RPCs an agent's CLI calls;
// a verdict snapshot published while blocked keeps that verdict after a
// repaired one is published; another agent reads neither the outcome nor the
// command output; and the outcome is the same after a Runner restart.
func TestDeliveryQualificationWithoutMission(t *testing.T) {
	source := history(t, "base")
	b, err := runner.New(runner.Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), WorktreeRoot: t.TempDir(), Address: "http://runner.invalid", Repositories: []collections.Repository{{ID: "api", Name: "api", Path: source}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	client := serve(t, b)
	ctx := timeout(t)
	ws, err := b.Collections.Create(ctx, "fixture", collections.Spec{Name: "catalog", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: git(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	agent := bearer(ctx, agentToken(t, fake.Session.Open.Env))
	owner := bearer(ctx, ownerToken)
	spec, err := b.Artifacts.Put(ws.ID, artifacts.Put{Title: "Catalog", Content: catalogSpec})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.GetDeliveryOutcome(agent, &pb.GetDeliveryOutcomeRequest{SessionId: e.ID}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("an outcome with no contract: %v", err)
	}
	if _, err := client.BindDelivery(agent, &pb.BindDeliveryRequest{SessionId: e.ID, ArtifactId: spec.ID, Version: 1}); err != nil {
		t.Fatal(err)
	}

	// The generator cannot run; an unrelated storage pass leaves it blocked.
	check(t, agent, client, e.ID, "generate", true, 1)
	storage := check(t, agent, client, e.ID, "storage", false, 0)
	outcome, err := client.GetDeliveryOutcome(agent, &pb.GetDeliveryOutcomeRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	head := outcome.Candidates[0]
	if !head.Current || head.Verdict != delivery.Blocked || states(head)["generate"] != delivery.Blocked || states(head)["storage"] != delivery.Passed || states(head)["api"] != delivery.Unrun {
		t.Fatalf("blocked outcome: %+v", outcome)
	}
	// People read the spec and each run by name, not by ID.
	if outcome.Contract.Title != "Catalog" || head.Criteria[1].Evidence[0].Title != "generate" {
		t.Fatalf("titles: %q %+v", outcome.Contract.Title, head.Criteria[1].Evidence)
	}
	if evidence := head.Criteria[0].Evidence; len(evidence) != 1 || evidence[0].CommandId != storage.Id || evidence[0].Output != "available" {
		t.Fatalf("storage evidence: %+v", head.Criteria[0])
	}
	blocked, err := client.PublishDeliveryOutcome(agent, &pb.PublishDeliveryOutcomeRequest{SessionId: e.ID})
	if err != nil || blocked.Kind != "report" || blocked.Title != "Delivery outcome · “Catalog” v1" || !strings.Contains(blocked.Content, `"verdict": "blocked"`) {
		t.Fatalf("publish while blocked: %v %v", blocked, err)
	}

	// Another agent reads neither this conversation's outcome nor its output.
	otherFake := &agenttest.Adapter{}
	b.Adapters["codex"] = otherFake
	if _, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	other := bearer(ctx, agentToken(t, otherFake.Session.Open.Env))
	if _, err := client.GetDeliveryOutcome(other, &pb.GetDeliveryOutcomeRequest{SessionId: e.ID}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("another agent read the outcome: %v", err)
	}
	if _, err := client.ReadCommand(other, &pb.ReadCommandRequest{CommandId: storage.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("another agent read the command output: %v", err)
	}

	// The repaired generator clears only its blocker; the API check completes it.
	check(t, agent, client, e.ID, "generate", false, 0)
	outcome, _ = client.GetDeliveryOutcome(agent, &pb.GetDeliveryOutcomeRequest{SessionId: e.ID})
	if head := outcome.Candidates[0]; head.Verdict != delivery.Incomplete || states(head)["api"] != delivery.Unrun || len(head.Criteria[1].Evidence) != 2 {
		t.Fatalf("partial repair: %+v", outcome)
	}
	check(t, agent, client, e.ID, "api", false, 0)
	passed, err := client.PublishDeliveryOutcome(agent, &pb.PublishDeliveryOutcomeRequest{SessionId: e.ID, ArtifactId: blocked.Id})
	if err != nil || len(passed.Versions) != 2 || !strings.Contains(passed.Content, `"verdict": "passed"`) {
		t.Fatalf("publish after repair: %v %v", passed, err)
	}
	first, err := client.GetArtifact(owner, &pb.GetArtifactRequest{CollectionId: ws.ID, ArtifactId: blocked.Id, Version: 1})
	if err != nil || !strings.Contains(first.Content, `"verdict": "blocked"`) {
		t.Fatalf("the first snapshot changed: %v %v", first, err)
	}
	if _, err := client.PublishDeliveryOutcome(agent, &pb.PublishDeliveryOutcomeRequest{SessionId: e.ID, ArtifactId: spec.ID}); err == nil {
		t.Fatal("published a verdict over the spec")
	}
	before, err := client.GetDeliveryOutcome(owner, &pb.GetDeliveryOutcomeRequest{SessionId: e.ID})
	if err != nil || before.Candidates[0].Verdict != delivery.Passed {
		t.Fatalf("final outcome: %v %v", before, err)
	}

	// A Runner restart serves the same outcome.
	if err := b.Detach(); err != nil {
		t.Fatal(err)
	}
	next, err := runner.New(b.Config)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = next.Close() })
	after, err := serve(t, next).GetDeliveryOutcome(owner, &pb.GetDeliveryOutcomeRequest{SessionId: e.ID})
	if err != nil || !proto.Equal(before, after) {
		t.Fatalf("restart changed the outcome:\n%v\n%v\n%v", before, after, err)
	}
}
