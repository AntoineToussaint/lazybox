package remote

import (
	"context"
	"encoding/json"
	"net/http/httptest"
	"os"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/transport"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
)

// phoneRunner stands in for execution behind the real device gateway: one
// conversation waiting for an approval, its journal, and a live stream that
// hears what Apply adds. Pairing, sign-in, credentials, the device allowlist
// and the Connect transcoding are the Runner's own code.
type phoneRunner struct {
	pb.UnimplementedRunnerServiceServer
	mu      sync.Mutex
	events  []*pb.JournalEvent
	waiting bool
	applied []*pb.ApplyRequest
	changed chan struct{}
	// final is the cursor that ends the turn the phone's Allow resumed; read
	// closes once a stream has sent the phone that far.
	final    uint64
	read     chan struct{}
	readOnce sync.Once
}

func newPhoneRunner() *phoneRunner {
	r := &phoneRunner{waiting: true, changed: make(chan struct{}), read: make(chan struct{})}
	r.add(&pb.AgentEvent{Type: "operation_accepted", Text: "Update the dependencies and run the tests.", OperationId: "op-1"})
	r.add(&pb.AgentEvent{Type: "turn_started"})
	r.add(&pb.AgentEvent{Type: "message_delta", Text: "Fourteen packages are behind. I'll update them and run the suite."})
	r.add(r.request())
	return r
}

func (r *phoneRunner) request() *pb.AgentEvent {
	return &pb.AgentEvent{Type: "approval", RequestId: "a-deps", Text: "Bash", ProviderDataJson: []byte(`{"input":{"command":"npm update --save"}}`)}
}

// add journals an event and wakes every stream; the caller holds no lock.
func (r *phoneRunner) add(event *pb.AgentEvent) {
	r.mu.Lock()
	r.events = append(r.events, &pb.JournalEvent{Cursor: uint64(len(r.events) + 1), CapturedAt: time.Now().UTC().Format(time.RFC3339Nano), Event: event})
	close(r.changed)
	r.changed = make(chan struct{})
	r.mu.Unlock()
}

func (r *phoneRunner) session() *pb.Session {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now().UnixMilli()
	s := &pb.Session{Id: "deps", ConversationId: "deps", CollectionId: "alpha", Title: "Bump dependencies", Provider: "claude", Model: "claude-opus-5-5", ModelReported: true,
		Status: "done", CreatedAtUnixMillis: now - 3_600_000, Agents: []*pb.AgentActivity{{Id: "root", Status: "done", UpdatedAtUnixMillis: now - 60_000}}}
	if r.waiting {
		s.Status, s.Agents[0].Status, s.Pending = "waiting_approval", "waiting_approval", []*pb.AgentEvent{r.request()}
	}
	return s
}

func (r *phoneRunner) Info(context.Context, *pb.InfoRequest) (*pb.InfoResponse, error) {
	return &pb.InfoResponse{Sessions: []*pb.Session{r.session()}}, nil
}
func (r *phoneRunner) ListCollections(context.Context, *pb.ListCollectionsRequest) (*pb.ListCollectionsResponse, error) {
	return &pb.ListCollectionsResponse{Collections: []*pb.Collection{{Id: "alpha", Name: "Release 2.4"}}}, nil
}
func (r *phoneRunner) GetSession(context.Context, *pb.SessionRequest) (*pb.SessionResponse, error) {
	return &pb.SessionResponse{Session: r.session()}, nil
}
func (r *phoneRunner) ReadSessionTail(context.Context, *pb.ReadSessionTailRequest) (*pb.ReadSessionTailResponse, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	return &pb.ReadSessionTailResponse{Events: append([]*pb.JournalEvent(nil), r.events...), Start: 1, Retained: uint64(len(r.events))}, nil
}
func (r *phoneRunner) WatchEvents(req *pb.WatchEventsRequest, stream grpc.ServerStreamingServer[pb.JournalEvent]) error {
	sent := req.GetAfter()
	for {
		r.mu.Lock()
		pending, wake := append([]*pb.JournalEvent(nil), r.events[min(int(sent), len(r.events)):]...), r.changed
		r.mu.Unlock()
		for _, event := range pending {
			if err := stream.Send(event); err != nil {
				return err
			}
			sent = event.Cursor
		}
		r.mu.Lock()
		if r.final > 0 && sent >= r.final {
			r.readOnce.Do(func() { close(r.read) })
		}
		r.mu.Unlock()
		if req.GetHeartbeats() {
			if err := stream.Send(&pb.JournalEvent{Cursor: sent, Retained: sent, Heartbeat: true}); err != nil {
				return err
			}
		}
		select {
		case <-stream.Context().Done():
			return nil
		case <-wake:
		case <-time.After(15 * time.Second):
		}
	}
}
func (r *phoneRunner) Apply(_ context.Context, req *pb.ApplyRequest) (*pb.Receipt, error) {
	r.mu.Lock()
	r.applied = append(r.applied, req)
	approval := req.GetApproval()
	if approval != nil {
		r.waiting = false
	}
	r.mu.Unlock()
	if approval != nil {
		r.add(&pb.AgentEvent{Type: "operation_accepted", RequestId: approval.RequestId, ProviderDataJson: []byte(`{"response":{"kind":"approve","decision":"accept"}}`)})
		r.add(&pb.AgentEvent{Type: "message_delta", Text: "Updated 14 packages. All 212 tests pass."})
		r.add(&pb.AgentEvent{Type: "turn_ended"})
		r.mu.Lock()
		r.final = uint64(len(r.events))
		r.mu.Unlock()
		// Wake the streams again: one may have sent the last event before final was set.
		r.add(&pb.AgentEvent{Type: "usage"})
	}
	return &pb.Receipt{OperationId: req.OperationId, Delivery: "accepted"}, nil
}

// TestSimulatorGateway serves the Runner's real device gateway on loopback for
// the iPhone app on the iOS Simulator, which shares the Mac's network
// (scripts/test-mobile-simulator.mjs --gateway real). It writes the pairing
// link to the file LAZYBOX_SIMULATOR_GATEWAY names, waits for the phone to
// pair with its own key and allow the request, and checks what arrived.
// Skipped unless that variable is set: it waits for an outside client.
func TestSimulatorGateway(t *testing.T) {
	out := os.Getenv("LAZYBOX_SIMULATOR_GATEWAY")
	if out == "" {
		t.Skip("set LAZYBOX_SIMULATOR_GATEWAY=<file> to serve the gateway to the simulator")
	}
	wait := 5 * time.Minute
	if v, err := strconv.Atoi(os.Getenv("LAZYBOX_SIMULATOR_SECONDS")); err == nil && v > 0 {
		wait = time.Duration(v) * time.Second
	}
	state := t.TempDir()
	m, err := devices.Open(state)
	if err != nil {
		t.Fatal(err)
	}
	r := newPhoneRunner()
	handler, stop, err := transport.DeviceHandler(r, m, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(stop)
	g, err := New(state, m, handler)
	if err != nil {
		t.Fatal(err)
	}
	server := httptest.NewServer(g) // 127.0.0.1, a port of its own
	t.Cleanup(server.Close)
	// The app keeps its live WatchEvents open; Close waits for open requests,
	// so end the phone's connections first (cleanups run last first).
	t.Cleanup(server.CloseClientConnections)
	offer, secret, err := m.Begin([]string{devices.GrantRead, devices.GrantRespond})
	if err != nil {
		t.Fatal(err)
	}
	link := server.URL + "/?view=remote#pair=" + offer.ID + "." + secret + "&grants=read%2Crespond"
	data, _ := json.Marshal(map[string]string{"gateway": server.URL, "link": link})
	if err = os.WriteFile(out, data, 0o600); err != nil {
		t.Fatal(err)
	}
	t.Logf("pairing link: %s", link)
	// Done once the phone has allowed the request and its stream has carried
	// the rest of the turn back to it; then a moment for the bytes in flight
	// to reach the app before the connections close.
	select {
	case <-r.read:
		time.Sleep(3 * time.Second)
	case <-time.After(wait):
		t.Fatalf("the phone did not allow the request and read the turn's end within %s", wait)
	}
	listed := m.List()
	if len(listed) != 1 {
		t.Fatalf("paired devices: %v", listed)
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	var approvals []*pb.Approval
	for _, a := range r.applied {
		if a.GetApproval() != nil {
			approvals = append(approvals, a.GetApproval())
		}
	}
	if len(approvals) != 1 || approvals[0].RequestId != "a-deps" || approvals[0].Decision != pb.Decision_DECISION_ACCEPT {
		t.Fatalf("approvals: %v", approvals)
	}
	t.Logf("paired %s (%s); approved %s", listed[0].Name, listed[0].Fingerprint(), approvals[0].RequestId)
}
