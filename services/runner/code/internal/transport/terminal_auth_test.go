package transport

import (
	"context"
	"encoding/json"
	"net"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
	"google.golang.org/protobuf/types/known/emptypb"
)

// terminalFixture serves a Runner with workspaces behind the production
// interceptors: two workspaces, a conversation in each, and a shell in the
// first.
type terminalFixture struct {
	service       *Service
	bench         *runner.Bench
	conn          *grpc.ClientConn
	client        pb.RunnerServiceClient
	fake          *agenttest.Adapter
	mine, theirs  *pb.Collection
	conversation  *runner.Execution // in mine
	agentToken    string            // conversation's own credential
	elsewhere     *runner.Execution // in theirs
	terminal      string
	terminalToken string
}

func newTerminalFixture(t *testing.T) *terminalFixture {
	t.Helper()
	f := newServedRunner(t)
	s, fake := f.service, f.fake
	ctx := context.Background()
	for i, name := range []string{"Parser", "Release"} {
		r := f.createWorkspace(t, name)
		e, err := s.Bench.OpenCollection("claude-default", r.Collection.Id, "", accounts.Options{}, "direct", "")
		if err != nil {
			t.Fatal(err)
		}
		if i == 0 {
			f.mine, f.conversation, f.agentToken = r.Collection, e, agentToken(t, fake.Session.Open.Env)
			if _, err = s.RenameConversation(ctx, &pb.RenameConversationRequest{SessionId: e.ID, Title: "Fix the parser"}); err != nil {
				t.Fatal(err)
			}
		} else {
			f.theirs, f.elsewhere = r.Collection, e
		}
	}
	f.terminal, f.terminalToken = f.openTerminal(t, f.mine.Id)
	return f
}

// newServedRunner is the Runner behind the production interceptors, reached
// over gRPC, with no workspace yet.
func newServedRunner(t *testing.T) *terminalFixture {
	t.Helper()
	t.Setenv("SHELL", "/bin/sh")
	t.Setenv("HOME", t.TempDir())
	t.Setenv("ENV", "")
	s, fake := collectionService(t)
	f := &terminalFixture{service: s, bench: s.Bench, fake: fake}
	server := grpc.NewServer(LocalAuthority(ownerToken, s.Bench)...)
	pb.RegisterRunnerServiceServer(server, s)
	listener := bufconn.Listen(1 << 20)
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { server.Stop(); listener.Close() })
	conn, err := grpc.NewClient("passthrough:///terminal", grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return listener.DialContext(ctx) }))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	f.conn, f.client = conn, pb.NewRunnerServiceClient(conn)
	return f
}

// createWorkspace creates a workspace with one member, api.
func (f *terminalFixture) createWorkspace(t *testing.T, name string) *pb.CollectionResponse {
	t.Helper()
	r, err := f.service.CreateCollection(context.Background(), &pb.CreateCollectionRequest{RequestKey: name, Spec: &pb.CollectionSpec{Name: name, Members: []*pb.WorktreeSpec{{Name: "api", RepositoryId: "api"}}}})
	if err != nil {
		t.Fatal(err)
	}
	return r
}

// openTerminal starts a shell and returns its id and the credential the shell
// finds through LAZYBOX_V2_ENDPOINT.
func (f *terminalFixture) openTerminal(t *testing.T, collection string) (string, string) {
	t.Helper()
	opened, err := f.service.OpenTerminal(context.Background(), &pb.OpenTerminalRequest{CollectionId: collection, Cols: 80, Rows: 24})
	if err != nil {
		t.Fatal(err)
	}
	path := f.bench.TerminalEndpoint(opened.Id)
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("terminal endpoint file must be private: %v %v", info, err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var endpoint struct{ URL, Token, CollectionID string }
	if err = json.Unmarshal(data, &endpoint); err != nil || !strings.HasPrefix(endpoint.Token, "lbx-terminal.") || endpoint.Token == ownerToken || endpoint.CollectionID != collection {
		t.Fatalf("terminal endpoint holds no scoped credential: %s %v", data, err)
	}
	return opened.Id, endpoint.Token
}

func TestTerminalCredentialPublishesOnlyToItsOwnWorkspace(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	shell := bearer(ctx, f.terminalToken)

	run, err := f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "go test ./..."})
	if err != nil {
		t.Fatal("own workspace refused", err)
	}
	if run.SessionId != "" || run.CollectionId != f.mine.Id || run.Origin != "terminal" {
		t.Fatalf("a terminal run belongs to its workspace: %+v", run)
	}
	if _, err = f.client.Publish(shell, &pb.PublishRequest{CommandId: run.Id, Data: []byte("--- FAIL: TestParse\n")}); err != nil {
		t.Fatal("own publish refused", err)
	}
	view := "[lazybox] go test ./... → piped from a terminal, exit status unknown\n--- FAIL: TestParse\n"
	if _, err = f.client.FinishPipe(shell, &pb.FinishPipeRequest{CommandId: run.Id, Report: &pb.CommandReport{Piped: true, OutputBytes: 20, AgentBytes: uint64(len(view)), AgentView: view}}); err != nil {
		t.Fatal("own finish refused", err)
	}

	// Every conversation of the workspace shows the run; another workspace's does not.
	owner := bearer(ctx, ownerToken)
	listed, err := f.client.ListCommands(owner, &pb.ListCommandsRequest{SessionId: f.conversation.ID})
	if err != nil || len(listed.Commands) != 1 || listed.Commands[0].Id != run.Id || listed.Commands[0].Report.GetAgentView() != "" {
		t.Fatalf("the workspace console lists the run without its view: %+v %v", listed, err)
	}
	if other, _ := f.client.ListCommands(owner, &pb.ListCommandsRequest{SessionId: f.elsewhere.ID}); len(other.Commands) != 0 {
		t.Fatalf("another workspace's console shows the run: %+v", other.Commands)
	}
	// The run's own status carries the compact view a person can send.
	if done, err := f.client.WaitCommand(owner, &pb.CommandRequest{CommandId: run.Id}); err != nil || done.Report.GetAgentView() != view {
		t.Fatalf("the run lost its compact view: %+v %v", done, err)
	}

	denied := func(what string, err error) {
		t.Helper()
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: got %v, want PermissionDenied", what, err)
		}
	}
	_, err = f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.theirs.Id, Title: "spoof"})
	denied("register in another workspace", err)
	_, err = f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{SessionId: f.conversation.ID, Title: "spoof"})
	denied("register in a conversation's own console", err)
	agentsPipe, err := f.client.RegisterPipe(bearer(ctx, f.agentToken), &pb.RegisterPipeRequest{SessionId: f.conversation.ID, Title: "agent"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.client.Publish(shell, &pb.PublishRequest{CommandId: agentsPipe.Id, Data: []byte("spoof")})
	denied("publish into an agent's pipe", err)
	_, err = f.client.FinishPipe(shell, &pb.FinishPipeRequest{CommandId: agentsPipe.Id})
	denied("finish an agent's pipe", err)
	// An agent cannot publish into the workspace, or into a terminal's run.
	_, err = f.client.RegisterPipe(bearer(ctx, f.agentToken), &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "spoof"})
	denied("agent registers a workspace run", err)
	_, err = f.client.RegisterPipe(bearer(ctx, f.agentToken), &pb.RegisterPipeRequest{SessionId: f.conversation.ID, CollectionId: f.mine.Id, ShareWith: f.conversation.ID, Title: "spoof"})
	denied("agent shares with a conversation", err)
	open, err := f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "open"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.client.Publish(bearer(ctx, f.agentToken), &pb.PublishRequest{CommandId: open.Id, Data: []byte("spoof")})
	denied("agent publishes into a terminal run", err)
	// A second shell in the same workspace cannot write into the first one's run.
	_, second := f.openTerminal(t, f.mine.Id)
	_, err = f.client.Publish(bearer(ctx, second), &pb.PublishRequest{CommandId: open.Id, Data: []byte("spoof")})
	denied("another terminal publishes into this run", err)

	// Nothing a terminal holds sends, approves, answers or reads conversations.
	_, err = f.client.Apply(shell, &pb.ApplyRequest{SessionId: f.conversation.ID, OperationId: "send", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "rm -rf"}}})
	denied("send a message", err)
	_, err = f.client.Apply(shell, &pb.ApplyRequest{SessionId: f.conversation.ID, OperationId: "approve", Operation: &pb.ApplyRequest_Approval{Approval: &pb.Approval{RequestId: "r", Decision: pb.Decision_DECISION_ACCEPT}}})
	denied("approve", err)
	_, err = f.client.ListHistory(shell, &pb.ListHistoryRequest{})
	denied("read history", err)
	_, err = f.client.ClaimCommandShare(shell, &pb.CommandRequest{CommandId: run.Id})
	denied("claim a share", err)
	select {
	case op := <-f.fake.Session.Applied:
		t.Fatalf("a terminal's operation reached the agent: %+v", op)
	default:
	}
}

func TestTerminalRunSharedWithAConversationIsClaimedOnce(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	shell := bearer(ctx, f.terminalToken)
	owner := bearer(ctx, ownerToken)

	conversation := f.conversation.Saved.ConversationID
	if conversation == "" {
		conversation = f.conversation.ID
	}
	// By title (any case, or a unique part of it) or by id; only in this workspace.
	for _, ref := range []string{"Fix the parser", "fix THE parser", "parser", f.conversation.ID} {
		run, err := f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "t", ShareWith: ref})
		if err != nil || run.ShareWith != conversation {
			t.Fatalf("share with %q: %+v %v", ref, run, err)
		}
		if _, err = f.client.FinishPipe(shell, &pb.FinishPipeRequest{CommandId: run.Id}); err != nil {
			t.Fatal(err)
		}
	}
	for _, ref := range []string{f.elsewhere.ID, "Release", "no such conversation"} {
		_, err := f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "t", ShareWith: ref})
		if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), `"Fix the parser"`) {
			t.Fatalf("share with %q outside the workspace must be refused, naming the choices: %v", ref, err)
		}
	}

	run, err := f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "make", ShareWith: "parser"})
	if err != nil {
		t.Fatal(err)
	}
	if claim, err := f.client.ClaimCommandShare(owner, &pb.CommandRequest{CommandId: run.Id}); err != nil || claim.Claimed {
		t.Fatalf("a run still being written must not be claimed: %+v %v", claim, err)
	}
	if _, err = f.client.FinishPipe(shell, &pb.FinishPipeRequest{CommandId: run.Id, Report: &pb.CommandReport{AgentView: "[lazybox] make → exit 2"}}); err != nil {
		t.Fatal(err)
	}
	claim, err := f.client.ClaimCommandShare(owner, &pb.CommandRequest{CommandId: run.Id})
	if err != nil || !claim.Claimed || claim.Status.Report.GetAgentView() != "[lazybox] make → exit 2" {
		t.Fatalf("first claim: %+v %v", claim, err)
	}
	if again, err := f.client.ClaimCommandShare(owner, &pb.CommandRequest{CommandId: run.Id}); err != nil || again.Claimed {
		t.Fatalf("a share is added to one message box only: %+v %v", again, err)
	}
	// A view larger than a report may carry is refused, not stored.
	big, _ := f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "big"})
	if _, err = f.client.FinishPipe(shell, &pb.FinishPipeRequest{CommandId: big.Id, Report: &pb.CommandReport{AgentView: strings.Repeat("x", 65<<10)}}); err == nil {
		t.Fatal("an agent view over 64 KiB was accepted")
	}
}

func TestTerminalCredentialIsRevokedWithItsShell(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	register := func(token string) error {
		_, err := f.client.RegisterPipe(bearer(ctx, token), &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "t"})
		return err
	}
	// Forged: another terminal's id under this MAC, an agent credential relabelled.
	forged := "lbx-terminal.OTHERTERMINAL" + f.terminalToken[strings.LastIndexByte(f.terminalToken, '.'):]
	relabelled := "lbx-terminal." + strings.TrimPrefix(f.agentToken, "lbx-agent.")
	for _, bad := range []string{forged, relabelled, f.terminalToken + "x", "lbx-terminal.", "lbx-terminal." + f.terminal + "."} {
		if err := register(bad); status.Code(err) != codes.Unauthenticated {
			t.Errorf("credential %q accepted: %v", bad, err)
		}
	}

	open, err := f.client.RegisterPipe(bearer(ctx, f.terminalToken), &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "still running"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.service.CloseTerminal(ctx, &pb.TerminalRequest{TerminalId: f.terminal}); err != nil {
		t.Fatal(err)
	}
	if err = register(f.terminalToken); status.Code(err) != codes.Unauthenticated {
		t.Fatal("a closed terminal's credential is still accepted", err)
	}
	if _, err = os.Stat(f.bench.TerminalEndpoint(f.terminal)); !os.IsNotExist(err) {
		t.Fatal("a closed terminal's endpoint file is kept", err)
	}
	if state, err := f.client.WaitCommand(bearer(ctx, ownerToken), &pb.CommandRequest{CommandId: open.Id}); err != nil || !state.Done || state.CaptureError == "" {
		t.Fatalf("a run its terminal left open must be finished, not left running: %+v %v", state, err)
	}

	// A shell that exits on its own revokes its credential too.
	exited, token := f.openTerminal(t, f.mine.Id)
	shell, err := f.bench.Terminals.Get(exited)
	if err != nil {
		t.Fatal(err)
	}
	if err = shell.Write([]byte("exit\n")); err != nil {
		t.Fatal(err)
	}
	if _, err = shell.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	if err = register(token); status.Code(err) != codes.Unauthenticated {
		t.Fatal("an exited shell's credential is still accepted", err)
	}

	// Deleting the workspace revokes every terminal credential in it.
	_, token = f.openTerminal(t, f.mine.Id)
	if err = register(token); err != nil {
		t.Fatal(err)
	}
	if _, err = f.service.DeleteWorkspace(ctx, &pb.DeleteWorkspaceRequest{CollectionId: f.mine.Id, ConfirmName: f.mine.Name, StopSessions: true, DiscardChanges: true}); err != nil {
		t.Fatal(err)
	}
	if err = register(token); status.Code(err) != codes.Unauthenticated {
		t.Fatal("a deleted workspace's terminal credential is still accepted", err)
	}
}

// Every RPC is denied to a terminal credential unless it is on the terminal
// allowlist, so an RPC added later is the owner's until someone decides otherwise.
func TestTerminalCredentialIsDeniedEveryRPCOffTheAllowlist(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	allowed := map[string]bool{}
	for method := range terminalRPCs {
		allowed[method] = true
	}
	if len(allowed) != 5 || !allowed[pb.RunnerService_RegisterPipe_FullMethodName] || !allowed[pb.RunnerService_Publish_FullMethodName] || !allowed[pb.RunnerService_FinishPipe_FullMethodName] ||
		!allowed[pb.RunnerService_FollowPipe_FullMethodName] || !allowed[pb.RunnerService_CancelCommand_FullMethodName] {
		t.Fatalf("terminal allowlist changed; review it deliberately: %v", allowed)
	}
	desc := pb.RunnerService_ServiceDesc
	checked := 0
	for _, m := range desc.Methods {
		method := "/" + desc.ServiceName + "/" + m.MethodName
		if allowed[method] {
			continue
		}
		if err := f.conn.Invoke(bearer(ctx, f.terminalToken), method, &emptypb.Empty{}, &emptypb.Empty{}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: terminal credential got %v, want PermissionDenied", method, err)
		}
		checked++
	}
	for _, s := range desc.Streams {
		method := "/" + desc.ServiceName + "/" + s.StreamName
		stream, err := f.conn.NewStream(bearer(ctx, f.terminalToken), &grpc.StreamDesc{StreamName: s.StreamName, ServerStreams: s.ServerStreams, ClientStreams: s.ClientStreams}, method)
		if err == nil {
			if err = stream.SendMsg(&emptypb.Empty{}); err == nil {
				_ = stream.CloseSend()
				err = stream.RecvMsg(&emptypb.Empty{})
			}
		}
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: terminal credential got %v, want PermissionDenied", method, err)
		}
		checked++
	}
	if checked != len(desc.Methods)+len(desc.Streams)-len(allowed) || checked < 60 {
		t.Fatalf("checked %d RPCs; the service lists %d", checked, len(desc.Methods)+len(desc.Streams))
	}
}

func TestTerminalCredentialSurvivesARunnerRestart(t *testing.T) {
	t.Setenv("SHELL", "/bin/sh")
	t.Setenv("HOME", t.TempDir())
	t.Setenv("ENV", "")
	state, worktrees := t.TempDir(), t.TempDir()
	first, err := runner.New(runner.Config{State: state, OutputFallbackRoot: t.TempDir(), WorktreeRoot: worktrees, Address: "http://first.invalid"})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	c, err := first.Collections.Create(ctx, "restart", collections.Spec{Name: "Restart"})
	if err != nil {
		t.Fatal(err)
	}
	term, err := first.OpenTerminal(c.ID, "", 80, 24, "")
	if err != nil {
		t.Fatal(err)
	}
	id := term.Status().ID
	before := readEndpointFile(t, first.TerminalEndpoint(id))
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	second, err := runner.New(runner.Config{State: state, OutputFallbackRoot: t.TempDir(), WorktreeRoot: worktrees, Address: "http://second.invalid"})
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	after := readEndpointFile(t, second.TerminalEndpoint(id))
	if after["url"] != "http://second.invalid" || after["token"] != before["token"] {
		t.Fatalf("the shell must reach the new Runner with the same credential: %v", after)
	}
	if terminal, workspace, err := second.TerminalScope(after["token"]); err != nil || terminal != id || workspace != c.ID {
		t.Fatalf("credential lost across the restart: %v", err)
	}
}

func readEndpointFile(t *testing.T, path string) map[string]string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var endpoint map[string]string
	if err = json.Unmarshal(data, &endpoint); err != nil {
		t.Fatal(err)
	}
	return endpoint
}
