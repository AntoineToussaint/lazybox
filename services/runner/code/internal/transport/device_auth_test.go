package transport

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"net"
	"net/http/httptest"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices/devicetest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
	"google.golang.org/protobuf/types/known/emptypb"
)

// deviceFixture serves one Runner twice, as work.go does: the owner's
// listener (LocalAuthority) and the device gateway's (DeviceAuthority).
type deviceFixture struct {
	*authorityFixture
	devices *devices.Manager
	service *Service
	conn    *grpc.ClientConn
	client  pb.RunnerServiceClient
}

func newDeviceFixture(t *testing.T) *deviceFixture {
	t.Helper()
	var service *Service
	f := &deviceFixture{authorityFixture: newAuthorityFixture(t, func(s *Service) { service = s })}
	m, err := devices.Open(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	service.Devices = m
	f.devices, f.service = m, service
	server := grpc.NewServer(DeviceAuthority(m)...)
	pb.RegisterRunnerServiceServer(server, service)
	listener := bufconn.Listen(1 << 20)
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { server.Stop(); listener.Close() })
	conn, err := grpc.NewClient("passthrough:///device", grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return listener.DialContext(ctx) }))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	f.conn, f.client = conn, pb.NewRunnerServiceClient(conn)
	return f
}

// pair pairs a phone with grants and signs it in, returning its device and
// access credential.
func (f *deviceFixture) pair(t *testing.T, grants ...string) (devices.Device, string) {
	t.Helper()
	p, err := devicetest.New()
	if err != nil {
		t.Fatal(err)
	}
	offer, secret, err := f.devices.Begin(grants)
	if err != nil {
		t.Fatal(err)
	}
	d, err := f.devices.Complete(p.PairRequest(offer.ID, secret, "phone"))
	if err != nil {
		t.Fatal(err)
	}
	challenge, _, err := f.devices.Challenge(d.ID)
	if err != nil {
		t.Fatal(err)
	}
	token, _, _, err := f.devices.Session(d.ID, challenge, p.Answer(d.ID, challenge))
	if err != nil {
		t.Fatal(err)
	}
	return d, token
}

func expectCode(t *testing.T, what string, err error, want codes.Code) {
	t.Helper()
	if status.Code(err) != want {
		t.Errorf("%s: got %v, want %v", what, err, want)
	}
}

// The device allowlist is pinned: adding to what a phone may do is a
// deliberate, reviewed change to this list and to REMOTE.md.
func TestDeviceAllowlistIsPinned(t *testing.T) {
	var got []string
	for method, rule := range deviceRPCs {
		got = append(got, strings.TrimPrefix(method, "/lazybox.bench.v1.RunnerService/")+":"+rule.grant)
	}
	for method, grant := range deviceStreams {
		got = append(got, strings.TrimPrefix(method, "/lazybox.bench.v1.RunnerService/")+":"+grant+":stream")
	}
	sort.Strings(got)
	want := []string{
		"AnswerConversationStart:respond", "Apply:respond", "GetCollection:read", "GetPushSettings:read", "GetSession:read", "ImportPullRequestReview:respond", "Info:read", "ListCollections:read", "ListCommands:read",
		"ListHistory:read", "ListLinks:read", "ListWorkspaceChanges:read", "OpenSession:start", "PinConversation:respond", "ReadAttachment:read", "ReadCommand:read", "ReadSessionTail:read", "ReadWorkspaceDiff:read",
		"ResumeSession:start", "SendPullRequestFailures:respond", "SendTestPush:read", "SetPushSettings:read", "WatchEvents:read:stream", "WatchOutput:read:stream",
	}
	if strings.Join(got, " ") != strings.Join(want, " ") {
		t.Fatalf("device allowlist changed; review it deliberately and update REMOTE.md:\n got %v\nwant %v", got, want)
	}
}

// Every RPC off the allowlist is denied to a device holding every grant, so
// an RPC added later is the owner's until someone decides otherwise.
func TestDeviceCredentialIsDeniedEveryRPCOffTheAllowlist(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	_, token := f.pair(t, devices.GrantRespond, devices.GrantStart)
	desc := pb.RunnerService_ServiceDesc
	checked := 0
	for _, m := range desc.Methods {
		method := "/" + desc.ServiceName + "/" + m.MethodName
		if _, ok := deviceRPCs[method]; ok {
			continue
		}
		err := f.conn.Invoke(bearer(ctx, token), method, &emptypb.Empty{}, &emptypb.Empty{})
		expectCode(t, method+" with a device credential", err, codes.PermissionDenied)
		err = f.conn.Invoke(ctx, method, &emptypb.Empty{}, &emptypb.Empty{})
		expectCode(t, method+" without a credential", err, codes.Unauthenticated)
		checked++
	}
	for _, s := range desc.Streams {
		method := "/" + desc.ServiceName + "/" + s.StreamName
		if _, ok := deviceStreams[method]; ok {
			continue
		}
		stream, err := f.conn.NewStream(bearer(ctx, token), &grpc.StreamDesc{StreamName: s.StreamName, ServerStreams: s.ServerStreams, ClientStreams: s.ClientStreams}, method)
		if err == nil {
			if err = stream.SendMsg(&emptypb.Empty{}); err == nil {
				_ = stream.CloseSend()
				err = stream.RecvMsg(&emptypb.Empty{})
			}
		}
		expectCode(t, method+" stream with a device credential", err, codes.PermissionDenied)
		checked++
	}
	if checked != len(desc.Methods)+len(desc.Streams)-len(deviceRPCs)-len(deviceStreams) || checked < 60 {
		t.Fatalf("checked %d RPCs; the service lists %d", checked, len(desc.Methods)+len(desc.Streams))
	}
	// The device-management RPCs are among them, explicitly.
	for _, method := range []string{pb.RunnerService_BeginDevicePairing_FullMethodName, pb.RunnerService_ListDevices_FullMethodName, pb.RunnerService_RevokeDevice_FullMethodName, pb.RunnerService_SetRemoteAccess_FullMethodName, pb.RunnerService_SaveRunnerSettings_FullMethodName, pb.RunnerService_ConnectGitHubPersonalToken_FullMethodName, pb.RunnerService_DeleteConversation_FullMethodName} {
		if _, ok := deviceRPCs[method]; ok {
			t.Fatalf("%s is on the device allowlist", method)
		}
	}
}

// Each grant opens exactly its RPCs: read is not respond, respond is not start.
func TestDeviceGrantsAreEnforcedPerRPC(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	e, fake, _ := f.open(t)
	_, reader := f.pair(t)
	_, responder := f.pair(t, devices.GrantRespond)
	_, starter := f.pair(t, devices.GrantRespond, devices.GrantStart)

	for _, token := range []string{reader, responder, starter} {
		if _, err := f.client.ListHistory(bearer(ctx, token), &pb.ListHistoryRequest{Limit: 10}); err != nil {
			t.Fatal("read refused", err)
		}
		if _, err := f.client.GetSession(bearer(ctx, token), &pb.SessionRequest{SessionId: e.ID}); err != nil {
			t.Fatal("read refused", err)
		}
	}
	message := &pb.ApplyRequest{SessionId: e.ID, OperationId: "from-phone", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "hello from the phone"}}}
	_, err := f.client.Apply(bearer(ctx, reader), message)
	expectCode(t, "read-only device sent a message", err, codes.PermissionDenied)
	if _, err = f.client.Apply(bearer(ctx, responder), message); err != nil {
		t.Fatal("responder refused", err)
	}
	select {
	case op := <-fake.Session.Applied:
		if op.Kind != agent.Message || op.Text != "hello from the phone" {
			t.Fatalf("unexpected operation %+v", op)
		}
	case <-ctx.Done():
		t.Fatal("phone's message not delivered")
	}
	// Raising the agent's permission mode is a setting, not a response.
	yolo := &pb.ApplyRequest{SessionId: e.ID, OperationId: "yolo", Operation: &pb.ApplyRequest_Permission{Permission: &pb.Permission{Mode: "yolo"}}}
	_, err = f.client.Apply(bearer(ctx, starter), yolo)
	expectCode(t, "device changed the permission mode", err, codes.PermissionDenied)

	// Starting needs its own grant, and only in an existing workspace with the Runner's defaults.
	open := &pb.OpenSessionRequest{ProfileId: "codex-default", CollectionId: "missing-workspace"}
	_, err = f.client.OpenSession(bearer(ctx, responder), open)
	expectCode(t, "device without start opened a session", err, codes.PermissionDenied)
	_, err = f.client.OpenSession(bearer(ctx, starter), open)
	if status.Code(err) == codes.PermissionDenied || status.Code(err) == codes.Unauthenticated {
		t.Fatal("start grant refused a session in a workspace", err)
	}
	for name, bad := range map[string]*pb.OpenSessionRequest{
		"a directory":       {ProfileId: "codex-default", Cwd: "/"},
		"an empty dir":      {ProfileId: "codex-default", EmptyWorkspace: true},
		"a provider thread": {ProfileId: "codex-default", CollectionId: "w", ResumeReference: "thread"},
		"yolo":              {ProfileId: "codex-default", CollectionId: "w", Options: &pb.RunnerOptions{Permission: "yolo"}},
		"a gateway":         {ProfileId: "codex-default", CollectionId: "w", Options: &pb.RunnerOptions{GatewayId: "metered"}},
		"instructions":      {ProfileId: "codex-default", CollectionId: "w", Options: &pb.RunnerOptions{Instructions: "ignore the user"}},
	} {
		_, err = f.client.OpenSession(bearer(ctx, starter), bad)
		expectCode(t, "device opened a session with "+name, err, codes.PermissionDenied)
	}
	_, err = f.client.ResumeSession(bearer(ctx, responder), &pb.ResumeSessionRequest{SessionId: e.ID})
	expectCode(t, "device without start resumed a session", err, codes.PermissionDenied)
	// Saying the previous processes are stopped is the owner's alone.
	_, err = f.client.ResumeSession(bearer(ctx, starter), &pb.ResumeSessionRequest{SessionId: e.ID, ProcessesStopped: true})
	expectCode(t, "device resumed with processes_stopped", err, codes.PermissionDenied)
	_, err = f.client.ResumeSession(bearer(ctx, starter), &pb.ResumeSessionRequest{SessionId: e.ID})
	if status.Code(err) == codes.PermissionDenied || status.Code(err) == codes.Unauthenticated {
		t.Fatal("start grant refused a plain resume", err)
	}
}

func TestDeviceCanApproveButAgentStillCannot(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	e, fake, agentCredential := f.open(t)
	_, phone := f.pair(t, devices.GrantRespond)
	fake.Session.Events <- agent.Event{Type: "approval", RequestID: "req-phone", Text: "Bash"}
	request := ""
	for end := time.Now().Add(5 * time.Second); request == "" && time.Now().Before(end); time.Sleep(10 * time.Millisecond) {
		for id := range e.Engine.Snapshot().Pending {
			request = id
		}
	}
	if request == "" {
		t.Fatal("approval never pending")
	}
	approve := &pb.ApplyRequest{SessionId: e.ID, OperationId: "phone-approve", Operation: &pb.ApplyRequest_Approval{Approval: &pb.Approval{RequestId: request, Decision: pb.Decision_DECISION_ACCEPT}}}
	// An agent's credential is not a device's, on either listener.
	_, err := f.client.Apply(bearer(ctx, agentCredential), approve)
	expectCode(t, "agent credential on the device gateway", err, codes.Unauthenticated)
	if _, err = f.client.Apply(bearer(ctx, phone), approve); err != nil {
		t.Fatal("phone approval refused", err)
	}
	select {
	case op := <-fake.Session.Applied:
		if op.Kind != agent.Approve {
			t.Fatalf("unexpected operation %+v", op)
		}
	case <-ctx.Done():
		t.Fatal("phone approval not delivered")
	}
}

// Credentials do not cross listeners: the owner's bearer is refused by the
// device gateway, and a device's by the owner's listener.
func TestCredentialsDoNotCrossListeners(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	_, phone := f.pair(t, devices.GrantRespond, devices.GrantStart)
	_, err := f.client.ListHistory(bearer(ctx, ownerToken), &pb.ListHistoryRequest{})
	expectCode(t, "owner bearer on the device gateway", err, codes.Unauthenticated)
	_, err = f.client.ListDevices(bearer(ctx, ownerToken), &pb.ListDevicesRequest{})
	expectCode(t, "owner bearer listed devices on the device gateway", err, codes.Unauthenticated)
	_, err = f.authorityFixture.client.ListHistory(bearer(ctx, phone), &pb.ListHistoryRequest{})
	expectCode(t, "device credential on the owner's listener", err, codes.Unauthenticated)
}

func TestRevokedDeviceIsRefusedAtItsNextCall(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	d, phone := f.pair(t)
	if _, err := f.client.ListHistory(bearer(ctx, phone), &pb.ListHistoryRequest{}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.service.RevokeDevice(ctx, &pb.RevokeDeviceRequest{DeviceId: d.ID}); err != nil {
		t.Fatal(err)
	}
	_, err := f.client.ListHistory(bearer(ctx, phone), &pb.ListHistoryRequest{})
	expectCode(t, "revoked device", err, codes.Unauthenticated)
	stream, err := f.client.WatchEvents(bearer(ctx, phone), &pb.WatchEventsRequest{SessionId: "x"})
	if err == nil {
		_, err = stream.Recv()
	}
	expectCode(t, "revoked device followed a conversation", err, codes.Unauthenticated)
}

// Revoking a device ends the streams it has open at once (audit: streams
// outlived revocation), not at its next request.
func TestRevokeEndsAnOpenDeviceStream(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	e, _, _ := f.open(t)
	d, phone := f.pair(t)
	stream, err := f.client.WatchEvents(bearer(ctx, phone), &pb.WatchEventsRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = stream.Recv(); err != nil {
		t.Fatal(err)
	}
	ended := make(chan error, 1)
	go func() {
		for {
			if _, err := stream.Recv(); err != nil {
				ended <- err
				return
			}
		}
	}()
	select {
	case err := <-ended:
		t.Fatalf("stream ended before revoke: %v", err)
	case <-time.After(100 * time.Millisecond):
	}
	if _, err = f.service.RevokeDevice(ctx, &pb.RevokeDeviceRequest{DeviceId: d.ID}); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-ended:
		expectCode(t, "stream of a revoked device", err, codes.Unauthenticated)
	case <-time.After(5 * time.Second):
		t.Fatal("the revoked device's stream is still open")
	}
}

// A stream lasts no longer than the credential that opened it: at its
// expiry it ends Unauthenticated, and the phone renews and reconnects.
func TestDeviceStreamEndsAtCredentialExpiry(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	e, _, _ := f.open(t)
	// A credential issued as if 15 minutes less 1 s ago expires in about a second.
	f.devices.Now = func() time.Time { return time.Now().Add(-devices.CredentialLifetime + 1500*time.Millisecond) }
	_, phone := f.pair(t)
	f.devices.Now = time.Now
	stream, err := f.client.WatchEvents(bearer(ctx, phone), &pb.WatchEventsRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	started := time.Now()
	for {
		if _, err = stream.Recv(); err != nil {
			break
		}
	}
	expectCode(t, "stream past its credential's expiry", err, codes.Unauthenticated)
	if waited := time.Since(started); waited > 5*time.Second {
		t.Fatalf("stream outlived its credential by %s", waited)
	}
}

// Info tells a device about conversations, not accounts: no usage, email,
// sign-in flows or gateways.
func TestDeviceInfoLeavesAccountsOnTheOwnersScreen(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	f.open(t)
	_, phone := f.pair(t)
	owner, err := f.authorityFixture.client.Info(bearer(ctx, ownerToken), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	got, err := f.client.Info(bearer(ctx, phone), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Sessions) != len(owner.Sessions) || len(got.Sessions) == 0 {
		t.Fatalf("device sees %d conversations, owner %d", len(got.Sessions), len(owner.Sessions))
	}
	if len(got.Gateways) != 0 || len(got.PendingEnrollments) != 0 || got.SharedSubscriptions || got.RestartAvailable {
		t.Fatalf("device info carries owner detail: %+v", got)
	}
	for _, p := range got.Profiles {
		if p.Availability != nil {
			t.Fatalf("device sees account usage for %s", p.Id)
		}
	}
}

// The device gateway's Connect endpoint dispatches through DeviceAuthority.
func TestDeviceScopeHoldsOverConnect(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	e, _, _ := f.open(t)
	_, phone := f.pair(t)
	handler, stop, err := DeviceHandler(f.service, f.devices, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(stop)
	host := httptest.NewServer(handler)
	t.Cleanup(host.Close)
	client := genconnect.NewRunnerServiceClient(host.Client(), host.URL)
	get := connect.NewRequest(&pb.SessionRequest{SessionId: e.ID})
	get.Header().Set("Authorization", "Bearer "+phone)
	if _, err = client.GetSession(ctx, get); err != nil {
		t.Fatal("device read refused over Connect", err)
	}
	remove := connect.NewRequest(&pb.DeleteConversationRequest{SessionId: e.ID, StopSessions: true})
	remove.Header().Set("Authorization", "Bearer "+phone)
	if _, err = client.DeleteConversation(ctx, remove); connect.CodeOf(err) != connect.CodePermissionDenied {
		t.Fatal("device deleted a conversation over Connect", err)
	}
	owner := connect.NewRequest(&pb.SessionRequest{SessionId: e.ID})
	owner.Header().Set("Authorization", "Bearer "+ownerToken)
	if _, err = client.GetSession(ctx, owner); connect.CodeOf(err) != connect.CodeUnauthenticated {
		t.Fatal("owner bearer accepted by the device gateway", err)
	}
}

// A phone reading a conversation sees the images its messages carried, under
// the same bounds as the owner: a retained message image, nothing else.
func TestDeviceReadsSentImagesAndTextWithinTheStoresBounds(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	execution, fake, _ := f.open(t)
	_, phone := f.pair(t)
	pixel, err := base64.StdEncoding.DecodeString("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jWZkAAAAASUVORK5CYII=")
	if err != nil {
		t.Fatal(err)
	}
	inputs := []*pb.FileAttachment{{Name: "screen.png", MediaType: "image/png", Data: pixel}, {Name: "notes.txt", MediaType: "text/plain", Data: []byte("plain")}, {Name: "archive.bin", Data: []byte{0x1f, 0x8b, 0, 0xff}}}
	if _, err = f.authorityFixture.client.Apply(bearer(ctx, ownerToken), &pb.ApplyRequest{SessionId: execution.ID, OperationId: "with-image", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "look", Attachments: inputs}}}); err != nil {
		t.Fatal(err)
	}
	<-fake.Session.Applied
	var files []agent.File
	for cursor := uint64(0); files == nil; {
		record, err := execution.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = record.Sequence
		var event agent.Event
		if json.Unmarshal(record.Value, &event) == nil && event.Type == "operation_accepted" {
			var data struct{ Files []agent.File }
			if err := json.Unmarshal(event.Data, &data); err != nil || len(data.Files) != 3 {
				t.Fatalf("accepted record lists no files: %s %v", event.Data, err)
			}
			files = data.Files
		}
	}
	got, err := f.client.ReadAttachment(bearer(ctx, phone), &pb.ReadAttachmentRequest{Path: files[0].Path})
	if err != nil || !bytes.Equal(got.Data, pixel) || got.MediaType != "image/png" {
		t.Fatalf("phone cannot read the sent image: %v", err)
	}
	// A text box's preview is served as plain text, like the owner's; a binary file is not.
	if got, err := f.client.ReadAttachment(bearer(ctx, phone), &pb.ReadAttachmentRequest{Path: files[1].Path}); err != nil || string(got.Data) != "plain" || got.MediaType != "text/plain; charset=utf-8" {
		t.Fatalf("phone cannot read the sent text as plain text: %v", err)
	}
	_, err = f.client.ReadAttachment(bearer(ctx, phone), &pb.ReadAttachmentRequest{Path: files[2].Path})
	expectCode(t, "a binary attachment served to a phone", err, codes.FailedPrecondition)
	_, err = f.client.ReadAttachment(bearer(ctx, phone), &pb.ReadAttachmentRequest{Path: filepath.Join(filepath.Dir(files[0].Path), "..", "..", "..", "..", "devices.json")})
	expectCode(t, "a path outside the attachment store", err, codes.InvalidArgument)
}
