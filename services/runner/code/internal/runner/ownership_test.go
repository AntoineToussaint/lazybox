package runner

import (
	"context"
	"encoding/json"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/procid"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// ownershipBench is a Runner with one workspace whose session "lost-owner" a
// previous Runner recorded and this one did not re-attach: its ownership is
// unconfirmed, exactly as after a Runner restart.
func ownershipBench(t *testing.T) (*Bench, collections.Collection) {
	t.Helper()
	repo := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Path: repo})
	cleanupHosts(t, b.Config.State)
	ctx := context.Background()
	c, err := b.Collections.Create(ctx, "ownership", collections.Spec{Name: "platform", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.Collections.Acquire(ctx, c.ID, "lost-owner", func(string) bool { return false }); err != nil {
		t.Fatal(err)
	}
	// The next Runner reads the workspace's record: its owner is unconfirmed.
	if err = b.Collections.Close(); err != nil {
		t.Fatal(err)
	}
	if b.Collections, err = collections.New(b.Config.WorktreeRoot, b.Config.Repositories); err != nil {
		t.Fatal(err)
	}
	if got, _ := b.Collections.Get(c.ID); !got.OwnershipUncertain {
		t.Fatal("fixture workspace is not unconfirmed")
	}
	return b, c
}

func uncertain(t *testing.T, b *Bench, id string) bool {
	t.Helper()
	c, err := b.Collections.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	return c.OwnershipUncertain
}

// The common case after the computer restarts: nothing the previous Runner
// recorded runs any more. The Runner proves it and clears the mark at
// startup, without asking anyone; a new conversation can then attach.
func TestOwnershipProvedGoneIsClearedWithoutAsking(t *testing.T) {
	b, c := ownershipBench(t)
	ctx := context.Background()
	if err := b.SettleProvenOwnership(ctx); err != nil {
		t.Fatal(err)
	}
	if uncertain(t, b, c.ID) {
		t.Fatal("a workspace whose recorded processes are all gone stayed unconfirmed")
	}
	got, _ := b.Collections.Get(c.ID)
	if !strings.Contains(got.LastOwnershipResolution, "lost-owner: Lazybox checked") {
		t.Fatalf("resolution %q does not say the Runner checked", got.LastOwnershipResolution)
	}
	if _, err := b.Collections.Acquire(ctx, c.ID, "next-owner", func(string) bool { return false }); err != nil {
		t.Fatalf("a new conversation still waits: %v", err)
	}
}

// Asking what is unconfirmed settles by itself what it proves gone, too.
func TestAskingWhatIsUnconfirmedSettlesWhatIsProvedGone(t *testing.T) {
	b, c := ownershipBench(t)
	o, err := b.WorkspaceOwnership(context.Background(), c.ID)
	if err != nil || !o.Settled || o.Note == "" || len(o.Processes) != 0 {
		t.Fatalf("ownership = %+v, %v; want settled by the Runner", o, err)
	}
	if uncertain(t, b, c.ID) {
		t.Fatal("still unconfirmed")
	}
}

// launchOwnedHost leaves a session host for owner running a program, with no
// Runner holding it, as a Runner that died would.
func launchOwnedHost(t *testing.T, root, owner string) (host string, program int) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	h, err := sessionhost.Launch(ctx, root, sessionhost.Options{Kind: "agent", Owner: owner, Label: "fixture agent"})
	if err != nil {
		t.Fatal(err)
	}
	if err = h.Spawn(ctx, sessionhost.Spec{Path: "/bin/sleep", Args: []string{"300"}, Cwd: t.TempDir()}); err != nil {
		t.Fatal(err)
	}
	program = h.AgentPID()
	host = h.ID()
	_ = h.Detach()
	return host, program
}

// A recorded agent still runs: the Runner shows it (PID, command line,
// running), and "Stop them and continue" stops exactly it, then clears the
// mark. An unrelated process is left alone.
func TestStopThemStopsTheRecordedProcessesThenClearsTheMark(t *testing.T) {
	b, c := ownershipBench(t)
	ctx := context.Background()
	_, program := launchOwnedHost(t, b.Config.HostRoot, "lost-owner")
	bystander := exec.Command("/bin/sleep", "300")
	if err := bystander.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = bystander.Process.Kill(); _ = bystander.Wait() })

	// Startup proves nothing here: it runs.
	if err := b.SettleProvenOwnership(ctx); err != nil {
		t.Fatal(err)
	}
	o, err := b.WorkspaceOwnership(ctx, c.ID)
	if err != nil || o.Settled || len(o.Processes) != 1 {
		t.Fatalf("ownership = %+v, %v; want the one running agent", o, err)
	}
	p := o.Processes[0]
	if p.Kind != "agent" || p.State != OwnedRunning || p.PID != program || !strings.Contains(p.CommandLine, "sleep 300") {
		t.Fatalf("recorded process shown as %+v; want running agent PID %d running sleep 300", p, program)
	}
	// The person's word is refused against what the Runner sees running.
	if _, err = b.ResolveWorkspaceOwnership(ctx, c.ID, false, ""); err == nil || !strings.Contains(err.Error(), "still running") {
		t.Fatalf("already-stopped accepted for a running process: %v", err)
	}
	o, err = b.ResolveWorkspaceOwnership(ctx, c.ID, true, "")
	if err != nil || !o.Settled {
		t.Fatalf("stop = %+v, %v", o, err)
	}
	stoppedEventually(t, program)
	if uncertain(t, b, c.ID) {
		t.Fatal("still unconfirmed after its processes were stopped")
	}
	if got, _ := b.Collections.Get(c.ID); !strings.Contains(got.LastOwnershipResolution, "stopped its processes") {
		t.Fatalf("resolution %q", got.LastOwnershipResolution)
	}
	if syscall.Kill(bystander.Process.Pid, 0) != nil {
		t.Fatal("an unrelated process was stopped")
	}
}

// "They're already stopped" records the person's word, as processes_stopped
// does on resume, when the Runner cannot check (a host that does not answer,
// whose start time was never recorded).
func TestAlreadyStoppedRecordsThePersonsWord(t *testing.T) {
	b, c := ownershipBench(t)
	listener := silentHost(t)
	bystander := exec.Command("/bin/sleep", "300")
	if err := bystander.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = bystander.Process.Kill(); _ = bystander.Wait() })
	writeHostRecord(t, b.Config.HostRoot, sessionhost.Record{Protocol: sessionhost.Protocol, ID: "unanswering-host", PID: bystander.Process.Pid, Port: listener, Token: "secret", Kind: "agent", Owner: "lost-owner"})
	ctx := context.Background()
	o, err := b.WorkspaceOwnership(ctx, c.ID)
	if err != nil || o.Settled || len(o.Processes) != 1 || o.Processes[0].State != OwnedUnknown {
		t.Fatalf("ownership = %+v, %v; want one process it cannot check", o, err)
	}
	// It cannot be stopped by identity: no start time.
	if o, err = b.ResolveWorkspaceOwnership(ctx, c.ID, true, ""); err == nil || o.Settled {
		t.Fatalf("stopped a process it cannot identify: %+v", o)
	}
	if syscall.Kill(bystander.Process.Pid, 0) != nil {
		t.Fatal("a process whose identity was unknown was killed")
	}
	if o, err = b.ResolveWorkspaceOwnership(ctx, c.ID, false, "I closed that terminal"); err != nil || !o.Settled {
		t.Fatalf("already stopped = %+v, %v", o, err)
	}
	got, _ := b.Collections.Get(c.ID)
	if got.OwnershipUncertain || !strings.Contains(got.LastOwnershipResolution, "You confirmed its processes had stopped: I closed that terminal") {
		t.Fatalf("workspace %+v", got)
	}
}

// A host that does not answer is stopped only if its PID still has the start
// time recorded for it. A PID now another program's is the recorded process
// gone: nothing is signalled, and the mark clears by itself.
func TestARecycledPIDIsNeverStoppedAndProvesTheProcessGone(t *testing.T) {
	b, c := ownershipBench(t)
	listener := silentHost(t)
	bystander := exec.Command("/bin/sleep", "300")
	if err := bystander.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = bystander.Process.Kill(); _ = bystander.Wait() })
	now, err := procid.Of(bystander.Process.Pid)
	if err != nil {
		t.Fatal(err)
	}
	// The record of an earlier process that had the bystander's number.
	writeHostRecord(t, b.Config.HostRoot, sessionhost.Record{Protocol: sessionhost.Protocol, ID: "recycled-host", PID: now.PID, Started: now.Start - 7_000_000, Port: listener, Token: "secret", Kind: "agent", Owner: "lost-owner"})
	ctx := context.Background()
	o, err := b.ResolveWorkspaceOwnership(ctx, c.ID, true, "")
	if err != nil || !o.Settled {
		t.Fatalf("stop = %+v, %v; want settled: the recorded process is gone", o, err)
	}
	time.Sleep(100 * time.Millisecond)
	if syscall.Kill(bystander.Process.Pid, 0) != nil {
		t.Fatal("the program that now has the recorded PID was killed")
	}
	if uncertain(t, b, c.ID) {
		t.Fatal("still unconfirmed")
	}
}

// The same host record with the start time that matches: it is the recorded
// process, alive but not answering, and Stop ends it by its identity.
func TestAnUnansweringHostIsStoppedByItsIdentity(t *testing.T) {
	b, c := ownershipBench(t)
	listener := silentHost(t)
	stuck := exec.Command("/bin/sleep", "300")
	if err := stuck.Start(); err != nil {
		t.Fatal(err)
	}
	waited := make(chan struct{})
	go func() { _ = stuck.Wait(); close(waited) }()
	t.Cleanup(func() { _ = stuck.Process.Kill(); <-waited })
	id, err := procid.Of(stuck.Process.Pid)
	if err != nil {
		t.Fatal(err)
	}
	writeHostRecord(t, b.Config.HostRoot, sessionhost.Record{Protocol: sessionhost.Protocol, ID: "stuck-host-1", PID: id.PID, Started: id.Start, Port: listener, Token: "secret", Kind: "agent", Owner: "lost-owner"})
	ctx := context.Background()
	o, err := b.WorkspaceOwnership(ctx, c.ID)
	if err != nil || len(o.Processes) != 1 || o.Processes[0].State != OwnedRunning || o.Processes[0].PID != id.PID {
		t.Fatalf("ownership = %+v, %v; want the stuck host running", o, err)
	}
	if o, err = b.ResolveWorkspaceOwnership(ctx, c.ID, true, ""); err != nil || !o.Settled {
		t.Fatalf("stop = %+v, %v", o, err)
	}
	select {
	case <-waited:
	case <-time.After(5 * time.Second):
		t.Fatal("the recorded process was not stopped")
	}
	if uncertain(t, b, c.ID) {
		t.Fatal("still unconfirmed")
	}
}

// A conversation a crashed Runner left open needs recovery; once the Runner
// proves its agent gone it is ended (resumable) and can be deleted, without
// the person vouching for anything.
func TestAConversationProvedGoneNeedsNoRecovery(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	b, err := New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	live := e.Engine.Snapshot()
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	store, err := sessionstore.OpenSQLite(filepath.Join(root, "sessions.db"))
	if err != nil {
		t.Fatal(err)
	}
	if err = store.SaveState(ctx, e.ID, live, nil); err != nil {
		t.Fatal(err)
	}
	store.Close()
	b, err = New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	if archived, _ := b.Get(e.ID); !archived.RecoveryRequired {
		t.Fatal("fixture conversation does not need recovery")
	}
	if err = b.DeleteConversation(ctx, e.ID, true); err == nil {
		t.Fatal("deleted a conversation whose processes are unconfirmed")
	}
	o, err := b.ConversationOwnership(ctx, e.ID)
	if err != nil || !o.Settled {
		t.Fatalf("ownership = %+v, %v; want settled by the Runner", o, err)
	}
	if again, _ := b.Get(e.ID); again.RecoveryRequired {
		t.Fatal("still needs recovery after the Runner proved its agent gone")
	}
	if events := recorded(t, b, e.ID); events[len(events)-1].Type != AgentLostEvent {
		t.Fatalf("no notice says what happened: %+v", events[len(events)-1])
	}
	if err = b.DeleteConversation(ctx, e.ID, true); err != nil {
		t.Fatal(err)
	}
}

// silentHost is an address that accepts a connection and closes it without a
// word: a host that does not answer, which is not proof that it is gone.
func silentHost(t *testing.T) int {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.Close() })
	go func() {
		for {
			c, err := l.Accept()
			if err != nil {
				return
			}
			_ = c.Close()
		}
	}()
	return l.Addr().(*net.TCPAddr).Port
}

func writeHostRecord(t *testing.T, root string, r sessionhost.Record) {
	t.Helper()
	dir := filepath.Join(root, r.ID)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	data, _ := json.Marshal(r)
	if err := os.WriteFile(filepath.Join(dir, "host.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	// Never stopped through the protocol by cleanupHosts: it is not a host.
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
}
