package power

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"
)

func dispatcherWith(t *testing.T, body string) (*Dispatcher, *[]string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "actions.yaml")
	if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	m := NewManager(path)
	if s := m.Snapshot(); s.Err != "" || len(s.Issues) != 0 {
		t.Fatalf("file: %+v", s)
	}
	var mu sync.Mutex
	launched := []string{}
	d := NewDispatcher(m, func(h Hook, e Event, run HookRun) (string, error) {
		mu.Lock()
		defer mu.Unlock()
		launched = append(launched, h.ID+":"+e.Name)
		if h.ID == "broken" {
			return "", errors.New("cannot start")
		}
		return "command-" + run.ID, nil
	})
	return d, &launched
}

func event(name, workspace, title string) Event {
	return Event{Name: name, Workspace: &Ref{ID: "w-" + workspace, Name: workspace}, Conversation: &Conversation{ID: "c", Session: "s", Title: title}, Key: name + title}
}

func TestDispatchMatchesEventsAndFilters(t *testing.T) {
	d, launched := dispatcherWith(t, `hooks:
  - id: all-input
    on: agent.needs-input
    run: [true]
  - id: release
    on: [turn.done, turn.failed]
    filter: { workspace: "release*", conversation: ["*deploy*"] }
    run: [true]
  - id: off
    on: turn.done
    run: [true]
    enabled: false
  - id: api-checks
    on: pr.checks-failed
    filter: { repository: acme/* }
    webhook: https://hooks.example.com
`)
	d.Dispatch(event(EventNeedsInput, "alpha", "x"))
	d.Dispatch(event(EventTurnDone, "Release 2", "Deploy staging"))
	d.Dispatch(event(EventTurnDone, "alpha", "deploy"))
	d.Dispatch(event(EventTurnFailed, "release", "refactor"))
	d.Dispatch(Event{Name: EventChecksFailed, Workspace: &Ref{ID: "w"}, PullRequest: &PullRequest{Repository: "acme/api", Number: 4, Head: "abc"}, Key: "pr"})
	d.Dispatch(Event{Name: EventChecksFailed, Workspace: &Ref{ID: "w"}, PullRequest: &PullRequest{Repository: "other/api", Number: 4, Head: "abc"}, Key: "pr2"})
	want := []string{"all-input:agent.needs-input", "release:turn.done", "api-checks:pr.checks-failed"}
	if !slices.Equal(*launched, want) {
		t.Fatalf("launched %v, want %v", *launched, want)
	}
}

func TestDispatchRunsOncePerEventAndBoundsRuns(t *testing.T) {
	d, launched := dispatcherWith(t, "hooks:\n  - id: h\n    on: turn.done\n    run: [true]\n")
	e := event(EventTurnDone, "alpha", "t")
	d.Dispatch(e)
	d.Dispatch(e)
	if len(*launched) != 1 {
		t.Fatalf("duplicate event ran %d times", len(*launched))
	}
	// Concurrency: four at once, the fifth is skipped and says why.
	for i := range 5 {
		e.Key = "k" + string(rune('a'+i))
		d.Dispatch(e)
	}
	runs := d.Runs(0)
	if runs[0].State != RunSkipped || !strings.Contains(runs[0].Error, "4 hooks are already running") {
		t.Fatalf("fifth concurrent run: %+v", runs[0])
	}
	for _, r := range runs {
		if r.State == RunRunning {
			d.Finish(r.ID, func(*HookRun) {})
		}
	}
	// Rate: ten a minute per hook.
	for i := range 10 {
		e.Key = "rate" + string(rune('a'+i))
		for _, r := range d.Dispatch(e) {
			d.Finish(r.ID, func(*HookRun) {})
		}
	}
	if r := d.Runs(1)[0]; r.State != RunSkipped || !strings.Contains(r.Error, "10 times") {
		t.Fatalf("rate limit: %+v", r)
	}
	// An event outside a workspace runs nothing (a hook needs a workspace to run in).
	runs = d.Dispatch(Event{Name: EventTurnDone, Key: "none"})
	if len(runs) != 1 || runs[0].State != RunSkipped {
		t.Fatalf("no workspace: %+v", runs)
	}
}

func TestDispatchRecordsLaunchFailuresAndOutcomes(t *testing.T) {
	d, _ := dispatcherWith(t, "hooks:\n  - id: broken\n    on: turn.done\n    run: [x]\n  - id: fine\n    on: turn.done\n    run: [x]\n    grants: [read]\n")
	runs := d.Dispatch(event(EventTurnDone, "alpha", "t"))
	if len(runs) != 2 || runs[0].State != RunFailed || runs[0].Error != "cannot start" || runs[1].State != RunRunning || runs[1].CommandID == "" || runs[1].Grants[0] != "read" {
		t.Fatalf("runs: %+v", runs)
	}
	code := 3
	d.Finish(runs[1].ID, func(r *HookRun) { r.State, r.ExitCode = RunFailed, &code })
	d.Finish(runs[1].ID, func(r *HookRun) { r.State = RunSucceeded }) // once only
	got := d.Runs(1)[0]
	if got.State != RunFailed || *got.ExitCode != 3 || got.Ended.IsZero() || got.Summary != "alpha · t" {
		t.Fatalf("finished: %+v", got)
	}
	test, err := d.Test("fine", Event{Workspace: &Ref{ID: "w"}})
	if err != nil || !test.Test || test.Event != EventTurnDone {
		t.Fatalf("test run: %+v %v", test, err)
	}
	if _, err := d.Test("missing", Event{}); err == nil {
		t.Fatal("tested a missing hook")
	}
}

func TestEnvironmentDropsRunnerWiringAndCarriesTheEvent(t *testing.T) {
	inherited := []string{"PATH=/bin", "HOME=/h", "LAZYBOX_V2_ENDPOINT=/owner/endpoint.json", "LAZYBOX_V2_STATE=/s", "CODEFLY_AUTH=x", "LAZYBOX_EVENT=forged", "SHLVL=3"}
	e := event(EventNeedsInput, "alpha", "fix `rm -rf /`; $(evil)")
	e.Request = &Request{ID: "r1", Kind: "approval"}
	env := Environment(inherited, e, "/state/hook-runs/x.json")
	joined := strings.Join(env, "\n")
	for _, gone := range []string{"/owner/endpoint.json", "LAZYBOX_V2_STATE", "CODEFLY_AUTH", "forged", "SHLVL"} {
		if strings.Contains(joined, gone) {
			t.Fatalf("environment keeps %s:\n%s", gone, joined)
		}
	}
	for _, want := range []string{"PATH=/bin", "HOME=/h", "LAZYBOX_EVENT=agent.needs-input", "LAZYBOX_EVENT_FILE=/state/hook-runs/x.json", "LAZYBOX_EVENT_WORKSPACE_NAME=alpha", "LAZYBOX_EVENT_REQUEST=r1", "LAZYBOX_EVENT_TITLE=fix `rm -rf /`; $(evil)"} {
		if !slices.Contains(env, want) {
			t.Fatalf("environment lacks %s:\n%s", want, joined)
		}
	}
}

func TestBrokerRoutesToTheWindowInFrontAndReturnsItsAnswer(t *testing.T) {
	b := NewBroker()
	if _, err := b.Post(UIRequest{Kind: UIOpen, SessionID: "s"}); !errors.Is(err, ErrNoWindow) {
		t.Fatalf("no window: %v", err)
	}
	web, stopWeb := b.Watch("web")
	defer stopWeb()
	if _, err := b.Post(UIRequest{Kind: UIShot}); err == nil || !strings.Contains(err.Error(), "desktop app") {
		t.Fatalf("shot in a browser: %v", err)
	}
	desktop, stopDesktop := b.Watch("desktop")
	posted, err := b.Post(UIRequest{Kind: UIShot, Panel: "changes"})
	if err != nil || posted.State != UIPending {
		t.Fatal(err)
	}
	select {
	case got := <-desktop:
		if got.ID != posted.ID || got.Panel != "changes" {
			t.Fatalf("delivered %+v", got)
		}
	case <-web:
		t.Fatal("a browser tab got the request while the desktop app is open")
	}
	go func() {
		time.Sleep(20 * time.Millisecond)
		if _, err := b.Complete(posted.ID, "", "Captured the window", []byte("png")); err != nil {
			t.Error(err)
		}
	}()
	done, err := b.Wait(context.Background(), posted.ID, "", 5*time.Second)
	if err != nil || done.State != UIDone || string(done.PNG) != "png" || done.Detail != "Captured the window" {
		t.Fatalf("wait: %+v %v", done, err)
	}
	if _, err := b.Complete(posted.ID, "", "", nil); err == nil {
		t.Fatal("answered twice")
	}
	// A window that closes fails what it had not answered.
	pending, _ := b.Post(UIRequest{Kind: UIOpen, CollectionID: "w"})
	stopDesktop()
	failed, _ := b.Wait(context.Background(), pending.ID, "", time.Second)
	if failed.State != UIFailed || !strings.Contains(failed.Error, "closed") {
		t.Fatalf("closed window: %+v", failed)
	}
	// The remaining browser tab takes the next request; waiting is bounded.
	next, err := b.Post(UIRequest{Kind: UIOpen, CollectionID: "w"})
	if err != nil {
		t.Fatal(err)
	}
	if got := <-web; got.ID != next.ID {
		t.Fatal("not routed to the remaining window")
	}
	still, _ := b.Wait(context.Background(), next.ID, "", 10*time.Millisecond)
	if still.State != UIPending {
		t.Fatalf("timeout: %+v", still)
	}
	if _, err := b.Post(UIRequest{Kind: "exec"}); err == nil {
		t.Fatal("unknown kind accepted")
	}
	if big, _ := b.Complete(next.ID, "", "", make([]byte, MaxShotBytes+1)); big.State != UIFailed {
		t.Fatalf("oversized shot: %+v", big)
	}
}
