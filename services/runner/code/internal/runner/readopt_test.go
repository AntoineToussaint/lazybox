package runner

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/claude"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// fullDisk is the Runner's telemetry database on a disk with no room left:
// SQLite answers every metering write with SQLITE_FULL.
type fullDisk struct {
	telemetry.Store
	full atomic.Bool
}

var errSQLiteFull = errors.New("database or disk is full (13)")

func (d *fullDisk) PutContext(ctx context.Context, c telemetry.ContextSnapshot) error {
	if d.full.Load() {
		return errSQLiteFull
	}
	return d.Store.PutContext(ctx, c)
}
func (d *fullDisk) Put(ctx context.Context, r telemetry.Record) error {
	if d.full.Load() {
		return errSQLiteFull
	}
	return d.Store.Put(ctx, r)
}

// approveAndFinish answers the pending approval and checks the agent process
// pid (the one that was always running) finishes the turn.
func approveAndFinish(t *testing.T, b *Bench, id string, pid int) []agent.Event {
	t.Helper()
	e, err := b.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "approve", Kind: agent.Approve, RequestID: "attempt-0:req-1", Decision: "accept"}); err != nil {
		t.Fatalf("the approval was refused: %v", err)
	}
	events := awaitEvent(t, b, id, "turn_ended")
	finished := ""
	for _, ev := range events {
		if ev.Type == "message_delta" {
			finished = ev.Text
		}
	}
	if finished != "accept by "+strconv.Itoa(pid) {
		t.Fatalf("turn not finished by the original agent process %d: %q", pid, finished)
	}
	s := e.Engine.Snapshot()
	if s.Fault != "" || s.Status == agent.Failed || s.Closed {
		t.Fatalf("the conversation did not end its turn as live: %+v", s)
	}
	return events
}

// The Dev failure: the Runner's metering database could not be written (the
// disk was full), and that ended its supervision of a live agent ("provider
// supervisor ended") and failed the conversation. A Runner write failure is
// reported, and the agent goes on under the same Runner.
func TestAFullDiskNeverEndsTheAgentsSupervision(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	b := fixtureBench(t, state)
	disk := &fullDisk{Store: b.Telemetry}
	disk.full.Store(true)
	b.Telemetry = disk
	t.Cleanup(func() { _ = b.Close() })
	e, err := b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "build", Kind: agent.Message, Text: "report context"}); err != nil {
		t.Fatal(err)
	}
	events := awaitEvent(t, b, e.ID, "approval")
	if count(events, agent.StorageErrorEvent) != 1 || count(events, "process_exit") != 0 {
		t.Fatalf("a metering write failure was not reported, or ended the agent: %+v", events)
	}
	if s := e.Engine.Snapshot(); s.Fault != "" {
		t.Fatalf("a metering write failure failed the conversation: %q", s.Fault)
	}
	pid := workerPID(t, events)
	disk.full.Store(false)
	approveAndFinish(t, b, e.ID, pid)
}

// One-time recovery: a conversation a Runner before this fix left failed
// although its agent kept running (the Dev journal 6TZV…: process_exit
// "database or disk is full (13)", process_exit "provider supervisor ended",
// fault "provider stream ended: provider supervisor ended") recovers when the
// next Runner re-adopts the live agent at startup.
func TestAConversationFailedByALostStreamRecoversWhenReadopted(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	first := fixtureBench(t, state)
	e, err := openAwaitingApproval(first)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, first, e.ID))
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	second := fixtureBench(t, state)
	t.Cleanup(func() { _ = second.Close() })
	// What the earlier Runner recorded when it lost the stream, as in Dev.
	ctx := context.Background()
	record, err := second.History.Get(ctx, e.ID)
	if err != nil {
		t.Fatal(err)
	}
	spool, err := journal.Open(filepath.Join(state, "sessions", e.ID+".jsonl"), second.Config.EventBudget, journal.Marking(turnStart))
	if err != nil {
		t.Fatal(err)
	}
	for _, text := range []string{"database or disk is full (13)", "provider supervisor ended"} {
		if _, err = spool.Append(agent.Event{Type: "process_exit", Status: "unknown", Text: text}); err != nil {
			t.Fatal(err)
		}
	}
	_ = spool.Dispose()
	stuck := record.Snapshot
	stuck.Fault = "provider stream ended: provider supervisor ended"
	root := stuck.Agents["root"]
	root.Status = agent.Unknown
	stuck.Agents["root"] = root
	if err = second.History.SaveState(ctx, e.ID, stuck, record.ProviderSettings); err != nil {
		t.Fatal(err)
	}

	if err = second.Adopt(ctx); err != nil {
		t.Fatal(err)
	}
	adopted, err := second.Get(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if s := adopted.Engine.Snapshot(); s.Fault != "" || s.Status == agent.Failed || s.Status == agent.Unknown || adopted.Archived {
		t.Fatalf("re-adopting the live agent left the conversation stuck: %+v", s)
	}
	if _, ok := adopted.Engine.Snapshot().Pending["attempt-0:req-1"]; !ok {
		t.Fatal("the pending approval was lost")
	}
	events := approveAndFinish(t, second, e.ID, pid)
	if count(events, "runner_reattached") != 1 {
		t.Fatalf("not one re-attachment: %+v", events)
	}
	// What is saved says so too: the next window that opens it sees it live.
	saved, err := second.History.Get(ctx, e.ID)
	if err != nil || saved.Snapshot.Fault != "" {
		t.Fatalf("the recovery was not saved: %q %v", saved.Snapshot.Fault, err)
	}
}

// cutAdapter is the Claude adapter with a stream this Runner can lose on
// demand while the agent goes on running in its host, as a Runner fault
// would (it once did on a full disk).
type cutAdapter struct {
	claude.Adapter
	cut chan error
}

func (a cutAdapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	s, err := a.Adapter.Open(ctx, o)
	if err != nil {
		return nil, err
	}
	return &cutSession{Session: s, cut: a.cut}, nil
}

type cutSession struct {
	agent.Session
	cut chan error
}

func (s *cutSession) Receive(ctx context.Context) (agent.Event, error) {
	inner, cancel := context.WithCancel(ctx)
	defer cancel()
	var cut error
	done := make(chan struct{})
	go func() {
		defer close(done)
		select {
		case cut = <-s.cut:
			cancel()
		case <-inner.Done():
		}
	}()
	ev, err := s.Session.Receive(inner)
	cancel()
	<-done
	if cut != nil {
		return agent.Event{}, cut
	}
	return ev, err
}
func (s *cutSession) DurableState() json.RawMessage { return s.Session.(agent.Durable).DurableState() }
func (s *cutSession) Commit(ev agent.Event)         { s.Session.(agent.Committer).Commit(ev) }
func (s *cutSession) Detach() error                 { return s.Session.(agent.Detacher).Detach() }
func (s *cutSession) HostID() string                { return agent.HostOf(s.Session) }

// Resume on a conversation whose only fault is a lost stream, while its agent
// still runs: the Runner re-attaches it in place. No second agent starts; the
// one that was running finishes its turn, in the same conversation.
func TestResumeReconnectsALiveAgentInPlace(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	b := fixtureBench(t, state)
	self, _ := os.Executable()
	cut := make(chan error, 1)
	b.Adapters["claude"] = cutAdapter{Adapter: claude.Adapter{Node: self, Worker: "fixture-claude-worker"}, cut: cut}
	t.Cleanup(func() { _ = b.Close() })
	e, err := openAwaitingApproval(b)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, b, e.ID))
	cut <- errors.New("session host connection lost: fixture")
	for end := time.Now().Add(10 * time.Second); !agent.TransportFault(e.Engine.Snapshot().Fault); time.Sleep(10 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatalf("the stream was never lost: %+v", e.Engine.Snapshot())
		}
	}
	if syscall.Kill(pid, 0) != nil {
		t.Fatal("losing the stream stopped the agent")
	}
	resumed, err := b.Resume(context.Background(), e.ID, false)
	if err != nil {
		t.Fatalf("Resume refused a live agent: %v", err)
	}
	if resumed.ID != e.ID {
		t.Fatalf("Resume started another execution %s instead of reconnecting %s", resumed.ID, e.ID)
	}
	if s := resumed.Engine.Snapshot(); s.Fault != "" || s.Closed {
		t.Fatalf("the reconnected conversation is not live: %+v", s)
	}
	if hosts, _ := sessionhost.Hosts(filepath.Join(state, "hosts")); len(hosts) != 1 {
		t.Fatalf("reconnecting left %d agent hosts", len(hosts))
	}
	approveAndFinish(t, b, e.ID, pid)
	// Again, now that nothing is wrong: Resume has nothing to do for a live
	// conversation and says so; it never starts a second agent.
	if _, err = b.Resume(context.Background(), e.ID, false); err == nil {
		t.Fatal("Resume of a healthy live conversation did something")
	}
	if hosts, _ := sessionhost.Hosts(filepath.Join(state, "hosts")); len(hosts) != 1 {
		t.Fatalf("Resume started a second agent: %d hosts", len(hosts))
	}
}
