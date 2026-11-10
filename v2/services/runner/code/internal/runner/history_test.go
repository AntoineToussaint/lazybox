package runner

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/sessionstore"
)

func waitHistory(t *testing.T, f func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !f() {
		if time.Now().After(deadline) {
			t.Fatal("state was not captured")
		}
		time.Sleep(time.Millisecond * 5)
	}
}
func TestRestartRestoresHistoryAndResumesOnceWithoutReplayingOrChangingDefaults(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	b, err := New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "message-one", Kind: agent.Message, Text: "Original conversation"}); err != nil {
		t.Fatal(err)
	}
	<-fake.Session.Applied
	fake.Session.Events <- agent.Event{Type: "message_delta", Text: "Retained reply", SessionID: "native-thread"}
	fake.Session.Events <- agent.Event{Type: "agent_status", AgentID: "child-one", ParentAgentID: "root", Status: "running", Name: "Research"}
	fake.Session.Events <- agent.Event{Type: "approval", RequestID: "old-approval"}
	waitHistory(t, func() bool { return len(e.Engine.Snapshot().Pending) == 1 })
	if err = os.WriteFile(filepath.Join(e.Cwd, "keep.txt"), []byte("kept"), 0600); err != nil {
		t.Fatal(err)
	}
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	b, err = New(Config{State: root, Defaults: accounts.Options{Permission: "read-only", Workflow: "plan", Model: "new-default"}})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if len(b.List()) != 0 {
		t.Fatal("startup launched a session")
	}
	restored, err := b.Get(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if !restored.Archived || len(restored.Engine.Snapshot().Pending) != 0 || restored.Engine.Snapshot().Queued != 0 {
		t.Fatal("restored actionable approvals/queue")
	}
	if _, err = restored.Engine.Apply(ctx, agent.Operation{ID: "old-approval", Kind: agent.Approve}); err == nil {
		t.Fatal("historical attempt accepted action")
	}
	fake = &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	var wg sync.WaitGroup
	results := make(chan *Execution, 2)
	failures := make(chan error, 2)
	for range 2 {
		wg.Add(1)
		go func() { defer wg.Done(); n, err := b.Resume(ctx, e.ID, false); results <- n; failures <- err }()
	}
	wg.Wait()
	for range 2 {
		if err = <-failures; err != nil {
			t.Fatal(err)
		}
	}
	first, second := <-results, <-results
	if first.ID != second.ID || first.Saved.ConversationID != e.ID || first.Cwd != e.Cwd {
		t.Fatal("duplicate or misplaced successor")
	}
	opened := fake.Session.Open
	if opened.Resume != "native-thread" || opened.Profile != "codex-default" || opened.Permission != "" || opened.Workflow != "" || opened.Model != "" || opened.Gateway != nil {
		t.Fatalf("resume changed saved options: %+v", opened)
	}
	select {
	case <-fake.Session.Applied:
		t.Fatal("resume replayed an operation")
	default:
	}
	if _, err = os.Stat(filepath.Join(e.Cwd, "keep.txt")); err != nil {
		t.Fatal(err)
	}
	var transcript strings.Builder
	for cursor := uint64(0); cursor < e.spool.Count(); cursor++ {
		r, err := b.Timeline(ctx, first.ID, cursor)
		if err != nil {
			t.Fatal(err)
		}
		transcript.Write(r.Value)
	}
	if !strings.Contains(transcript.String(), "Retained reply") || !strings.Contains(transcript.String(), "Original conversation") {
		t.Fatal("conversation history lost on resume")
	}
	records, err := b.History.List(ctx, 20, 0)
	if err != nil || len(records) != 1 || records[0].ID != first.ID {
		t.Fatal("history did not group attempts", err)
	}
}
func TestCrashRecoveryRequiresProcessReconciliationAndRejectsChangedAccount(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	b, err := New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	live := e.Engine.Snapshot()
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	// Crash fixture: the database contains an open attempt, with no claim that
	// its process exited. The test does not leave a real orphan on the machine.
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
	archived, err := b.Get(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if !archived.RecoveryRequired || archived.Engine.Snapshot().Status != agent.Interrupted {
		t.Fatal("unclean attempt not marked interrupted")
	}
	if _, err = b.Resume(ctx, e.ID, false); err == nil {
		t.Fatal("unconfirmed writer resumed")
	}
	b.profileMu.Lock()
	b.profiles[0].Home = filepath.Join(root, "different-account")
	b.profileMu.Unlock()
	if _, err = b.Resume(ctx, e.ID, true); err == nil {
		t.Fatal("changed account binding accepted")
	}
}

type failCreate struct{ sessionstore.Store }

func (f failCreate) Create(context.Context, sessionstore.Record) error {
	return errors.New("injected durable write failure")
}
func TestStorageFailurePreventsProviderStartup(t *testing.T) {
	root := t.TempDir()
	store, err := sessionstore.OpenSQLite(filepath.Join(root, "sessions.db"))
	if err != nil {
		t.Fatal(err)
	}
	b, err := New(Config{State: root, SessionStore: failCreate{store}})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err == nil {
		t.Fatal("missing write failure")
	}
	if fake.Session != nil {
		t.Fatal("provider started before durable identity")
	}
}
func TestSessionStoreRoundTripPolicyAndSingleSuccessor(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "sessions.db")
	store, err := sessionstore.OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	if other, err := sessionstore.OpenSQLite(path); err == nil {
		other.Close()
		t.Fatal("second owner accepted")
	}
	r := sessionstore.Record{ID: "a", ConversationID: "a", ProfileID: "account", Provider: "codex"}
	if err = store.Create(ctx, r); err != nil {
		t.Fatal(err)
	}
	settings := json.RawMessage(`{"approvalPolicy":"untrusted","model":"pinned"}`)
	if err = store.SaveState(ctx, "a", engine.Snapshot{ID: "a", Reference: "native-id"}, settings); err != nil {
		t.Fatal(err)
	}
	if err = store.Create(ctx, sessionstore.Record{ID: "b", ConversationID: "a", Previous: "a"}); err != nil {
		t.Fatal(err)
	}
	if err = store.Create(ctx, sessionstore.Record{ID: "c", ConversationID: "a", Previous: "a"}); err == nil {
		t.Fatal("duplicate successor")
	}
	store.Close()
	store, err = sessionstore.OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	saved, err := store.Get(ctx, "a")
	if err != nil || saved.Snapshot.Reference != "native-id" || string(saved.ProviderSettings) != string(settings) {
		t.Fatal("policy or native reference lost", err)
	}
	if _, err = store.Get(ctx, "missing"); !errors.Is(err, os.ErrNotExist) {
		t.Fatal(err)
	}
	if _, err = store.List(ctx, 101, 0); err == nil {
		t.Fatal("unbounded history")
	}
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatal("session database permissions", err)
	}
}

func TestMeteredResumeResolvesNewGatewayAfterRestart(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	b, err := New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "metered")
	if err != nil {
		t.Fatal(err)
	}
	oldURL := fake.Session.Open.Gateway.URL
	fake.Session.Events <- agent.Event{Type: "message_delta", Text: "hello", SessionID: "native-reference"}
	waitHistory(t, func() bool { return e.Engine.Snapshot().Reference == "native-reference" })
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	b, err = New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake = &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	next, err := b.Resume(ctx, e.ID, false)
	if err != nil {
		t.Fatal(err)
	}
	opened := fake.Session.Open
	if opened.Gateway == nil || opened.Gateway.ID != "metered" || opened.Gateway.URL == oldURL || opened.Resume != "native-reference" || opened.Permission != "" || next.Saved.ConversationID != e.ID {
		t.Fatalf("bad resume: %+v", opened)
	}
}

func TestHistoryIdentityScopedSearchAndNamesSurviveContinuationAndRestart(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "sessions.db")
	store, err := sessionstore.OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	identity, err := store.Identity(ctx)
	if err != nil || identity == "" {
		t.Fatal(identity, err)
	}
	for _, r := range []sessionstore.Record{{ID: "a", ConversationID: "a", CollectionID: "alpha", Title: "First"}, {ID: "b", ConversationID: "b", CollectionID: "beta", Title: "Other"}, {ID: "c", ConversationID: "c", CollectionID: "alpha", Title: "Hidden older"}} {
		if err := store.Create(ctx, r); err != nil {
			t.Fatal(err)
		}
	}
	if err := store.SaveState(ctx, "c", engine.Snapshot{Title: "Actual first message"}, nil); err != nil {
		t.Fatal(err)
	}
	nativeRows, err := store.Query(ctx, sessionstore.Query{Limit: 10, Search: "first message"})
	if err != nil || len(nativeRows) != 1 || nativeRows[0].ID != "c" {
		t.Fatal("native title search", nativeRows, err)
	}
	if err := store.Rename(ctx, "a", "My 100% _release"); err != nil {
		t.Fatal(err)
	}
	// Native state writes must not overwrite the user's conversation name.
	if err := store.SaveState(ctx, "a", engine.Snapshot{Title: "native first prompt"}, nil); err != nil {
		t.Fatal(err)
	}
	if err := store.Create(ctx, sessionstore.Record{ID: "a2", Previous: "a", ConversationID: "a", CollectionID: "alpha", Title: "First"}); err != nil {
		t.Fatal(err)
	}
	workspace := "alpha"
	rows, err := store.Query(ctx, sessionstore.Query{Limit: 1, Workspace: &workspace, Search: "100% _"})
	if err != nil || len(rows) != 1 || rows[0].ID != "a2" || rows[0].UserTitle != "My 100% _release" {
		t.Fatal(rows, err)
	}
	workspace = "beta"
	rows, err = store.Query(ctx, sessionstore.Query{Limit: 10, Workspace: &workspace, Search: "100%"})
	if err != nil || len(rows) != 0 {
		t.Fatal(rows, err)
	}
	for _, name := range []string{"", strings.Repeat("x", 121), "bad\nname"} {
		if err := store.Rename(ctx, "a", name); err == nil {
			t.Fatal("invalid title accepted")
		}
	}
	if err := store.Rename(ctx, "missing", "Title"); err == nil {
		t.Fatal("missing conversation renamed")
	}
	store.Close()
	store, err = sessionstore.OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	restored, err := store.Identity(ctx)
	if err != nil || restored != identity {
		t.Fatal(restored, err)
	}
	record, err := store.Get(ctx, "a2")
	if err != nil || record.UserTitle != "My 100% _release" {
		t.Fatal(record, err)
	}
}
