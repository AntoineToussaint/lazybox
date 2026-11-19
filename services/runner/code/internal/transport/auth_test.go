package transport

import (
	"context"
	"encoding/json"
	"net"
	"net/http/httptest"
	"os"
	"testing"
	"time"

	"connectrpc.com/connect"
	"connectrpc.com/vanguard/vanguardgrpc"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
	"google.golang.org/protobuf/types/known/emptypb"
)

const ownerToken = "owner-fixture-token"

// authorityFixture serves a real Runner behind the production interceptors.
type authorityFixture struct {
	bench  *runner.Bench
	server *grpc.Server
	conn   *grpc.ClientConn
	client pb.RunnerServiceClient
}

func newAuthorityFixture(t *testing.T, configure ...func(*Service)) *authorityFixture {
	t.Helper()
	b, err := runner.New(runner.Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), Address: "http://runner.invalid"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	server := grpc.NewServer(LocalAuthority(ownerToken, b)...)
	service := &Service{Bench: b}
	for _, c := range configure {
		c(service)
	}
	pb.RegisterRunnerServiceServer(server, service)
	listener := bufconn.Listen(1 << 20)
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { server.Stop(); listener.Close() })
	conn, err := grpc.NewClient("passthrough:///authority", grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return listener.DialContext(ctx) }))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	return &authorityFixture{bench: b, server: server, conn: conn, client: pb.NewRunnerServiceClient(conn)}
}

// open starts an execution on a fake provider and returns it with the
// credential its agent finds through LAZYBOX_V2_ENDPOINT.
func (f *authorityFixture) open(t *testing.T) (*runner.Execution, *agenttest.Adapter, string) {
	t.Helper()
	fake := &agenttest.Adapter{}
	f.bench.Adapters["codex"] = fake
	e, err := f.bench.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	return e, fake, agentToken(t, fake.Session.Open.Env)
}

func agentToken(t *testing.T, env []string) string {
	t.Helper()
	path := ""
	for _, entry := range env {
		if len(entry) > len("LAZYBOX_V2_ENDPOINT=") && entry[:len("LAZYBOX_V2_ENDPOINT=")] == "LAZYBOX_V2_ENDPOINT=" {
			path = entry[len("LAZYBOX_V2_ENDPOINT="):]
		}
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal("agent endpoint unreadable", err)
	}
	var endpoint struct{ URL, Token string }
	if err = json.Unmarshal(data, &endpoint); err != nil || endpoint.Token == "" || endpoint.Token == ownerToken {
		t.Fatalf("agent endpoint holds no scoped credential: %v", err)
	}
	return endpoint.Token
}

func bearer(ctx context.Context, token string) context.Context {
	return metadata.AppendToOutgoingContext(ctx, "authorization", "Bearer "+token)
}

func timeout(t *testing.T) context.Context {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func TestAgentCredentialPublishesOnlyItsOwnSessionsConsole(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	mine, _, token := f.open(t)
	other, _, otherToken := f.open(t)
	agent := bearer(ctx, token)

	pipe, err := f.client.RegisterPipe(agent, &pb.RegisterPipeRequest{SessionId: mine.ID, Title: "go test"})
	if err != nil {
		t.Fatal("own console refused", err)
	}
	if _, err = f.client.Publish(agent, &pb.PublishRequest{CommandId: pipe.Id, Data: []byte("ok\n")}); err != nil {
		t.Fatal("own publish refused", err)
	}
	if _, err = f.client.FinishPipe(agent, &pb.FinishPipeRequest{CommandId: pipe.Id, Report: &pb.CommandReport{ExitCode: 0}}); err != nil {
		t.Fatal("own finish refused", err)
	}

	if _, err = f.client.RegisterPipe(agent, &pb.RegisterPipeRequest{SessionId: other.ID, Title: "spoof"}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("agent registered a pipe in another session", err)
	}
	theirs, err := f.client.RegisterPipe(bearer(ctx, otherToken), &pb.RegisterPipeRequest{SessionId: other.ID, Title: "theirs"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.Publish(agent, &pb.PublishRequest{CommandId: theirs.Id, Data: []byte("spoof")}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("agent published into another session's pipe", err)
	}
	if _, err = f.client.FinishPipe(agent, &pb.FinishPipeRequest{CommandId: theirs.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("agent finished another session's pipe", err)
	}
	if _, err = f.client.Publish(agent, &pb.PublishRequest{CommandId: "missing", Data: []byte("x")}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("unknown command not denied", err)
	}
	// The owner keeps full authority.
	if _, err = f.client.FinishPipe(bearer(ctx, ownerToken), &pb.FinishPipeRequest{CommandId: theirs.Id}); err != nil {
		t.Fatal("owner refused", err)
	}
	if _, err = f.client.ListCommands(bearer(ctx, ownerToken), &pb.ListCommandsRequest{SessionId: mine.ID}); err != nil {
		t.Fatal("owner refused", err)
	}
}

// Replaces the 2026-11-09 audit probe (tests/audit/2026-11-09/
// audit_authority_test.go.txt): the credential an agent can find must not
// approve or answer its own pending request through the authenticated RPC.
func TestAuditAgentCredentialCannotApproveOrAnswer(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	e, fake, token := f.open(t)
	fake.Session.Events <- agent.Event{Type: "approval", RequestID: "req-1", Text: "Bash"}
	request := ""
	for end := time.Now().Add(5 * time.Second); request == "" && time.Now().Before(end); time.Sleep(10 * time.Millisecond) {
		for id := range e.Engine.Snapshot().Pending {
			request = id
		}
	}
	if request == "" {
		t.Fatal("approval never pending")
	}
	approve := &pb.ApplyRequest{SessionId: e.ID, OperationId: "self-approve", Operation: &pb.ApplyRequest_Approval{Approval: &pb.Approval{RequestId: request, Decision: pb.Decision_DECISION_ACCEPT}}}
	if _, err := f.client.Apply(bearer(ctx, token), approve); status.Code(err) != codes.PermissionDenied {
		t.Fatal("AUDIT: agent credential approved its own request", err)
	}
	answer := &pb.ApplyRequest{SessionId: e.ID, OperationId: "self-answer", Operation: &pb.ApplyRequest_Answer{Answer: &pb.Answer{RequestId: request}}}
	if _, err := f.client.Apply(bearer(ctx, token), answer); status.Code(err) != codes.PermissionDenied {
		t.Fatal("AUDIT: agent credential answered its own request", err)
	}
	select {
	case op := <-fake.Session.Applied:
		t.Fatalf("an agent's decision reached the provider: %+v", op)
	default:
	}
	if _, ok := e.Engine.Snapshot().Pending[request]; !ok {
		t.Fatal("request resolved without the owner")
	}
	// The same request from the owner is delivered.
	if _, err := f.client.Apply(bearer(ctx, ownerToken), approve); err != nil {
		t.Fatal("owner approval refused", err)
	}
	select {
	case op := <-fake.Session.Applied:
		if op.Kind != agent.Approve {
			t.Fatalf("unexpected operation %+v", op)
		}
	case <-ctx.Done():
		t.Fatal("owner approval not delivered")
	}
}

// Every RPC is denied to an agent credential unless it is on the agent
// allowlist, so an RPC added later is the owner's until someone decides otherwise.
func TestAgentCredentialIsDeniedEveryRPCOffTheAllowlist(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	_, _, token := f.open(t)
	desc := pb.RunnerService_ServiceDesc
	allowed := map[string]bool{}
	for method := range agentRPCs {
		allowed[method] = true
	}
	// Console publishing and following for its own execution, listing and
	// stopping its own commands (background_runs_test.go), its own
	// workspace's artifacts, and retiring the ones agents published there
	// (each rule tested in artifacts_test.go), and
	// asking for a repository in its own workspace (repository_requests_test.go),
	// and a reviewer filing, listing and settling its own review's findings, and the two agents
	// of a review asking each other (review_runs_test.go, review_loop_test.go),
	// and the Orchestrator verbs, scoped to a grant and the caller's own
	// lineage (agents_test.go, TestOrchestratorGrantWalksEveryRPC), and
	// linking and listing its own workspace's pull requests (items_test.go),
	// and asking the person to start a conversation and waiting for its own
	// request's answer (conversation_starts_test.go).
	// Reviewer-only verification execution is covered by TestVerificationRPCScopesRecipesAndCancelsProcesses.
	if len(allowed) != 33 || !allowed[pb.RunnerService_RunReviewVerification_FullMethodName] || !allowed[pb.RunnerService_RetireArtifact_FullMethodName] || !allowed[pb.RunnerService_RequestConversationStart_FullMethodName] || !allowed[pb.RunnerService_WaitConversationStart_FullMethodName] || !allowed[pb.RunnerService_ListLinks_FullMethodName] || !allowed[pb.RunnerService_LinkItem_FullMethodName] || !allowed[pb.RunnerService_ClaimLease_FullMethodName] || !allowed[pb.RunnerService_ReleaseLease_FullMethodName] || !allowed[pb.RunnerService_ListLeases_FullMethodName] || !allowed[pb.RunnerService_ProposePlan_FullMethodName] || !allowed[pb.RunnerService_SpawnAgent_FullMethodName] || !allowed[pb.RunnerService_ListAgentLineage_FullMethodName] || !allowed[pb.RunnerService_SendAgentMessage_FullMethodName] ||
		!allowed[pb.RunnerService_WaitAgents_FullMethodName] || !allowed[pb.RunnerService_StopChildAgent_FullMethodName] || !allowed[pb.RunnerService_ReportAgentTask_FullMethodName] || !allowed[pb.RunnerService_SetReviewFindingState_FullMethodName] || !allowed[pb.RunnerService_AskAgent_FullMethodName] || !allowed[pb.RunnerService_WaitAgentAsk_FullMethodName] || !allowed[pb.RunnerService_AddReviewFinding_FullMethodName] || !allowed[pb.RunnerService_ListReviewFindings_FullMethodName] || !allowed[pb.RunnerService_RegisterPipe_FullMethodName] || !allowed[pb.RunnerService_Publish_FullMethodName] || !allowed[pb.RunnerService_FinishPipe_FullMethodName] ||
		!allowed[pb.RunnerService_FollowPipe_FullMethodName] || !allowed[pb.RunnerService_CancelCommand_FullMethodName] || !allowed[pb.RunnerService_ListCommands_FullMethodName] ||
		!allowed[pb.RunnerService_ListArtifacts_FullMethodName] || !allowed[pb.RunnerService_GetArtifact_FullMethodName] || !allowed[pb.RunnerService_PutArtifact_FullMethodName] ||
		!allowed[pb.RunnerService_ListWorkspaceRepositories_FullMethodName] || !allowed[pb.RunnerService_RequestRepository_FullMethodName] || !allowed[pb.RunnerService_WaitRepositoryRequest_FullMethodName] {
		t.Fatalf("agent allowlist changed; review it deliberately: %v", allowed)
	}
	checked := 0
	for _, m := range desc.Methods {
		method := "/" + desc.ServiceName + "/" + m.MethodName
		if allowed[method] {
			continue
		}
		// An empty message decodes as any request type, so this reaches the
		// interceptor with the RPC's own request message.
		err := f.conn.Invoke(bearer(ctx, token), method, &emptypb.Empty{}, &emptypb.Empty{})
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: agent credential got %v, want PermissionDenied", method, err)
		}
		if err = f.conn.Invoke(ctx, method, &emptypb.Empty{}, &emptypb.Empty{}); status.Code(err) != codes.Unauthenticated {
			t.Errorf("%s: no credential got %v, want Unauthenticated", method, err)
		}
		checked++
	}
	for _, s := range desc.Streams {
		method := "/" + desc.ServiceName + "/" + s.StreamName
		stream, err := f.conn.NewStream(bearer(ctx, token), &grpc.StreamDesc{StreamName: s.StreamName, ServerStreams: s.ServerStreams, ClientStreams: s.ClientStreams}, method)
		if err == nil {
			if err = stream.SendMsg(&emptypb.Empty{}); err == nil {
				_ = stream.CloseSend()
				err = stream.RecvMsg(&emptypb.Empty{})
			}
		}
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: agent credential got %v, want PermissionDenied", method, err)
		}
		checked++
	}
	if checked != len(desc.Methods)+len(desc.Streams)-len(allowed) || checked < 60 {
		t.Fatalf("checked %d RPCs; the service lists %d", checked, len(desc.Methods)+len(desc.Streams))
	}
}

func TestForgedOrRevokedAgentCredentialIsUnauthenticated(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	e, _, token := f.open(t)
	other, _, _ := f.open(t)
	register := func(token, session string) error {
		_, err := f.client.RegisterPipe(bearer(ctx, token), &pb.RegisterPipeRequest{SessionId: session, Title: "t"})
		return err
	}
	// Re-binding a credential to another execution breaks its MAC.
	forged := "lbx-agent." + other.ID + token[len("lbx-agent.")+len(e.ID):]
	for _, bad := range []string{forged, token + "x", "lbx-agent.", "lbx-agent." + e.ID + ".", ownerToken + "x", ""} {
		if err := register(bad, other.ID); status.Code(err) != codes.Unauthenticated {
			t.Errorf("credential %q accepted: %v", bad, err)
		}
	}
	if err := f.bench.DeleteConversation(ctx, e.ID, true); err != nil {
		t.Fatal(err)
	}
	if err := register(token, e.ID); status.Code(err) != codes.Unauthenticated {
		t.Fatal("deleted conversation's credential still accepted", err)
	}
	if _, err := os.Stat(f.bench.AgentEndpoint(e.ID)); !os.IsNotExist(err) {
		t.Fatal("deleted conversation's endpoint file kept", err)
	}
}

// The Connect listener dispatches through the same interceptors.
func TestAgentCredentialScopeHoldsOverConnect(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	e, _, token := f.open(t)
	transcoder, err := vanguardgrpc.NewTranscoder(f.server)
	if err != nil {
		t.Fatal(err)
	}
	host := httptest.NewServer(transcoder)
	t.Cleanup(host.Close)
	client := genconnect.NewRunnerServiceClient(host.Client(), host.URL)
	register := connect.NewRequest(&pb.RegisterPipeRequest{SessionId: e.ID, Title: "connect"})
	register.Header().Set("Authorization", "Bearer "+token)
	if _, err = client.RegisterPipe(ctx, register); err != nil {
		t.Fatal("own console refused over Connect", err)
	}
	remove := connect.NewRequest(&pb.DeleteConversationRequest{SessionId: e.ID, StopSessions: true})
	remove.Header().Set("Authorization", "Bearer "+token)
	if _, err = client.DeleteConversation(ctx, remove); connect.CodeOf(err) != connect.CodePermissionDenied {
		t.Fatal("agent deleted its conversation over Connect", err)
	}
}
