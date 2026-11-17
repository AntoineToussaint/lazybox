package transport

import (
	"os"
	"path/filepath"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/emptypb"
)

// installExtension writes an extension folder and approves what it asks for.
func installExtension(t *testing.T, f *powerFixture, id, manifest string) *pb.ExtensionPanelSession {
	t.Helper()
	folder := filepath.Join(f.service.Bench.Extensions().Dir(), id)
	if err := os.MkdirAll(folder, 0o700); err != nil {
		t.Fatal(err)
	}
	for name, data := range map[string]string{"extension.yaml": manifest, "index.html": "<!doctype html>"} {
		if err := os.WriteFile(filepath.Join(folder, name), []byte(data), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	list, err := f.client.ListExtensions(f.owner(t), &pb.ListExtensionsRequest{})
	if err != nil {
		t.Fatal(err)
	}
	var ext *pb.Extension
	for _, e := range list.Extensions {
		if e.Id == id {
			ext = e
		}
	}
	if ext == nil || ext.State != "new" {
		t.Fatalf("listed %+v", list.Extensions)
	}
	// A panel of an extension no one approved does not open.
	if _, err := f.client.OpenExtensionPanel(f.owner(t), &pb.OpenExtensionPanelRequest{Id: id}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("opened unapproved: %v", err)
	}
	if _, err := f.client.ApproveExtension(f.owner(t), &pb.ApproveExtensionRequest{Id: id, Grants: ext.Grants, Hosts: ext.Hosts}); err != nil {
		t.Fatal(err)
	}
	session, err := f.client.OpenExtensionPanel(f.owner(t), &pb.OpenExtensionPanelRequest{Id: id, Origin: "http://127.0.0.1:5173"})
	if err != nil {
		t.Fatal(err)
	}
	return session
}

// An extension's credential reaches exactly the RPCs its approved grants
// name, with the requests they allow, and nothing else: every other RPC
// (every owner-only one, the extension RPCs, Info, any added later) and every
// stream is denied. Turning the extension off revokes it.
func TestExtensionCredentialReachesOnlyItsGrants(t *testing.T) {
	f := newPowerFixture(t, "")
	session := installExtension(t, f, "board", `name: Board
panels: [{id: board, title: Board}]
grants: [read:attention, show:conversations, do:inbox.open]
`)
	if session.Credential == ownerToken || len(session.Credential) < 40 {
		t.Fatalf("credential %q", session.Credential)
	}
	ext := bearer(timeout(t), session.Credential)
	if _, err := f.client.ListHistory(ext, &pb.ListHistoryRequest{}); err != nil {
		t.Fatal("read:attention cannot list conversations:", err)
	}
	// Showing a conversation, and the one action it was approved for, reach the window.
	owner := f.owner(t)
	watch, err := f.client.WatchUiRequests(owner, &pb.WatchUiRequestsRequest{Surface: "desktop"})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = watch.Recv()
	for _, ok := range []*pb.UiRequest{{Kind: "open", SessionId: f.session}, {Kind: "do", ActionId: "inbox.open"}} {
		posted, err := f.client.PostUiRequest(ext, ok)
		if err != nil {
			t.Fatalf("%s %s refused: %v", ok.Kind, ok.ActionId, err)
		}
		if got, _ := watch.Recv(); got.Id != posted.Id {
			t.Fatalf("delivered %+v", got)
		}
		if _, err = f.client.CompleteUiRequest(owner, &pb.CompleteUiRequestRequest{Id: posted.Id, Detail: "Shown"}); err != nil {
			t.Fatal(err)
		}
		if done, err := f.client.WaitUiRequest(ext, &pb.WaitUiRequestRequest{Id: posted.Id, TimeoutMs: 1000}); err != nil || done.Detail != "Shown" {
			t.Fatalf("wait own: %+v %v", done, err)
		}
	}
	// An owner's request is not the extension's to wait on.
	mine, err := f.client.PostUiRequest(owner, &pb.UiRequest{Kind: "open", SessionId: f.session})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = watch.Recv()
	if _, err := f.client.WaitUiRequest(ext, &pb.WaitUiRequestRequest{Id: mine.Id, TimeoutMs: 200}); err == nil {
		t.Fatal("the extension waited on the owner's request")
	}
	// What it was not given: another action (even a safe one), an unsafe
	// one, a file, a panel, a screenshot, a user action, a message.
	for _, refused := range []*pb.UiRequest{{Kind: "do", ActionId: "attention.next"}, {Kind: "do", ActionId: "settings.open"}, {Kind: "open", SessionId: f.session, File: "README.md"}, {Kind: "open", Panel: "settings"}, {Kind: "shot"}, {Kind: "action", ActionId: "x"}} {
		if _, err := f.client.PostUiRequest(ext, refused); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s %s: %v, want PermissionDenied", refused.Kind, refused.ActionId, err)
		}
	}
	send := &pb.ApplyRequest{SessionId: f.session, OperationId: "ext-send", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "hi"}}}
	if _, err := f.client.Apply(ext, send); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("sent a message without send:messages: %v", err)
	}
	// The walk: everything else is denied.
	// WaitUiRequest answers only its own requests (above).
	allowed := map[string]bool{pb.RunnerService_ListHistory_FullMethodName: true, pb.RunnerService_WaitUiRequest_FullMethodName: true}
	desc := pb.RunnerService_ServiceDesc
	checked := 0
	for _, m := range desc.Methods {
		method := "/" + desc.ServiceName + "/" + m.MethodName
		if allowed[method] {
			continue
		}
		if err := f.conn.Invoke(ext, method, &emptypb.Empty{}, &emptypb.Empty{}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: extension credential got %v, want PermissionDenied", method, err)
		}
		checked++
	}
	for _, s := range desc.Streams {
		method := "/" + desc.ServiceName + "/" + s.StreamName
		stream, err := f.conn.NewStream(ext, &grpc.StreamDesc{StreamName: s.StreamName, ServerStreams: s.ServerStreams}, method)
		if err == nil {
			if err = stream.SendMsg(&emptypb.Empty{}); err == nil {
				_ = stream.CloseSend()
				err = stream.RecvMsg(&emptypb.Empty{})
			}
		}
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: extension credential got %v, want PermissionDenied", method, err)
		}
	}
	if checked < 150 {
		t.Fatalf("checked only %d RPCs", checked)
	}
	// Forged, and revoked.
	if _, err := f.client.ListHistory(bearer(timeout(t), session.Credential+"x"), &pb.ListHistoryRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("forged: %v", err)
	}
	if _, err := f.client.SetExtensionEnabled(f.owner(t), &pb.SetExtensionEnabledRequest{Id: "board", Enabled: false}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.ListHistory(bearer(timeout(t), session.Credential), &pb.ListHistoryRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("a disabled extension's credential still works: %v", err)
	}
}

// Each grant opens its RPCs and only the requests it allows: a message, never
// an interrupt or a permission change; commands of one conversation.
func TestExtensionGrantsScopeRequests(t *testing.T) {
	f := newPowerFixture(t, "")
	session := installExtension(t, f, "all", `name: All
panels: [{id: main, title: Main}]
grants: [read:workspaces, read:sessions, read:commands, read:missions, read:usage, send:messages]
`)
	ext := bearer(timeout(t), session.Credential)
	if _, err := f.client.ListCollections(ext, &pb.ListCollectionsRequest{}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.ListMissions(ext, &pb.ListMissionsRequest{}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.GetTelemetry(ext, &pb.TelemetryRequest{Workspace: f.workspace.Id}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.ListCommands(ext, &pb.ListCommandsRequest{SessionId: f.session}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.ListCommands(ext, &pb.ListCommandsRequest{}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("every workspace's commands: %v", err)
	}
	if _, err := f.client.Apply(ext, &pb.ApplyRequest{SessionId: f.session, OperationId: "m1", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "from the extension"}}}); err != nil {
		t.Fatal("send:messages cannot send:", err)
	}
	for _, op := range []*pb.ApplyRequest{
		{SessionId: f.session, OperationId: "i1", Operation: &pb.ApplyRequest_Interrupt{Interrupt: &pb.Interrupt{}}},
		{SessionId: f.session, OperationId: "p1", Operation: &pb.ApplyRequest_Permission{Permission: &pb.Permission{}}},
	} {
		if _, err := f.client.Apply(ext, op); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%T: %v", op.Operation, err)
		}
	}
	// Reading a conversation's messages is never a grant.
	if _, err := f.client.GetSession(ext, &pb.SessionRequest{SessionId: f.session}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("read a conversation: %v", err)
	}
	if _, err := f.client.PostUiRequest(ext, &pb.UiRequest{Kind: "open", SessionId: f.session}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("showed without show:conversations: %v", err)
	}
}

// An approval is of exactly what the sheet showed.
func TestApproveExtensionNeedsWhatTheSheetShowed(t *testing.T) {
	f := newPowerFixture(t, "")
	folder := filepath.Join(f.service.Bench.Extensions().Dir(), "x")
	_ = os.MkdirAll(folder, 0o700)
	_ = os.WriteFile(filepath.Join(folder, "extension.yaml"), []byte("name: X\npanels: [{id: main, title: Main}]\ngrants: [read:sessions, send:messages]\n"), 0o600)
	_ = os.WriteFile(filepath.Join(folder, "index.html"), nil, 0o600)
	if _, err := f.client.ApproveExtension(f.owner(t), &pb.ApproveExtensionRequest{Id: "x", Grants: []string{"read:sessions"}}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("partial approval: %v", err)
	}
	if _, err := f.client.ApproveExtension(f.owner(t), &pb.ApproveExtensionRequest{Id: "nope"}); status.Code(err) != codes.NotFound {
		t.Fatalf("missing: %v", err)
	}
	list, err := f.client.InstallExampleExtension(f.owner(t), &pb.InstallExampleExtensionRequest{Example: "agent-board"})
	if err != nil || len(list.Examples) != 3 {
		t.Fatalf("%+v %v", list, err)
	}
	if _, err := f.client.InstallExampleExtension(f.owner(t), &pb.InstallExampleExtensionRequest{Example: "agent-board"}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("installed twice: %v", err)
	}
	if _, err := f.client.RemoveExtension(f.owner(t), &pb.RemoveExtensionRequest{Id: "agent-board"}); err != nil {
		t.Fatal(err)
	}
}
