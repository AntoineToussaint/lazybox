package items

import (
	"errors"
	"path/filepath"
	"testing"
	"time"
)

func openStore(t *testing.T) *Store {
	t.Helper()
	store, err := Open(filepath.Join(t.TempDir(), "items.json"))
	if err != nil {
		t.Fatal(err)
	}
	return store
}

// An item that was never read and whose reads fail (a pull request that does
// not exist, or that the connection cannot see: 404) backs off like any
// other; it is not asked about on every pass.
func TestA404ItemBacksOff(t *testing.T) {
	now := time.Now()
	missing := TrackedItem{Item: Item{Ref: GitHubRef("acme/api", 404)}, Tracking: Tracking{CheckedAt: now.Add(-90 * time.Second), Failures: 1, LastError: "HTTP 404"}}
	if due, next := Due(missing, now, true); due || !next.Equal(missing.Tracking.CheckedAt.Add(2*OpenFocused)) {
		t.Fatalf("one failure: due %v, next %v", due, next)
	}
	missing.Tracking.Failures = 10
	missing.Tracking.CheckedAt = now.Add(-29 * time.Minute)
	if due, _ := Due(missing, now, true); due {
		t.Fatal("ten failures: up to the 30-minute cap")
	}
	missing.Tracking.CheckedAt = now.Add(-30 * time.Minute)
	if due, _ := Due(missing, now, true); !due {
		t.Fatal("after the cap it is asked again")
	}
	// Never read yet: at once.
	if due, _ := Due(TrackedItem{Item: Item{Ref: GitHubRef("acme/api", 1)}}, now, true); !due {
		t.Fatal("an item never read is due at once")
	}

	// Through the store: a failed read of an unread item backs it off.
	store := openStore(t)
	ref := GitHubRef("acme/api", 404)
	if _, err := store.Link(Link{Ref: ref, Workspace: "w1", Role: RoleDelivers, Source: SourcePerson}); err != nil {
		t.Fatal(err)
	}
	if err := store.Failed(ref, errors.New("HTTP 404")); err != nil {
		t.Fatal(err)
	}
	if due, _ := Due(store.Tracked()[0], store.Now(), true); due {
		t.Fatal("a 404 item is read again on the next pass")
	}
}

// "." and ".." are never a repository: in an API path they would name
// another endpoint with the connection's token.
func TestDotRepositoryNamesAreRefused(t *testing.T) {
	for _, url := range []string{"https://github.com/acme/../pull/1", "https://github.com/acme/./pull/1", "https://github.com/acme/.git/pull/1", "https://github.com/acme/..git/issues/2"} {
		if r, ok := ResolveGitHub(url); ok {
			t.Errorf("%s resolved to %+v", url, r)
		}
		if found := Scan("see "+url, true); len(found) != 0 {
			t.Errorf("%s scanned to %+v", url, found)
		}
	}
	for _, key := range []string{"acme/..#1", "acme/.#1", "../api#1"} {
		if _, _, ok := ParseGitHubKey(key); ok {
			t.Errorf("key %q parsed", key)
		}
	}
	if r, ok := ResolveGitHub("https://github.com/acme/.github/pull/3"); !ok || r.Repository != "acme/.github" {
		t.Fatalf("a dot-prefixed name is a repository: %+v %v", r, ok)
	}
}

// What agents say a workspace delivers is bounded per workspace and per
// conversation; the person's and the branch detector's links are not counted.
func TestAgentDeliversAreBounded(t *testing.T) {
	store := openStore(t)
	for i := 1; i <= agentDeliversPerSession; i++ {
		if _, err := store.Link(Link{Ref: GitHubRef("acme/api", int64(i)), Workspace: "w1", Session: "s1", Role: RoleDelivers, Source: SourceCLI}); err != nil {
			t.Fatalf("link %d: %v", i, err)
		}
	}
	if _, err := store.Link(Link{Ref: GitHubRef("acme/api", 999), Workspace: "w1", Session: "s1", Role: RoleDelivers, Source: SourceAgentOutput}); !errors.Is(err, ErrTooManyDelivered) {
		t.Fatalf("past the conversation's bound: %v", err)
	}
	// A mention upgraded to delivers counts too.
	if _, err := store.Link(Link{Ref: GitHubRef("acme/api", 998), Workspace: "w1", Session: "s1", Role: RoleMentions, Source: SourceAgentOutput}); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Link(Link{Ref: GitHubRef("acme/api", 998), Workspace: "w1", Session: "s1", Role: RoleDelivers, Source: SourceCLI}); !errors.Is(err, ErrTooManyDelivered) {
		t.Fatalf("an upgrade past the bound: %v", err)
	}
	// Other conversations fill the workspace's bound.
	n := 1000
	for s := 2; ; s++ {
		session := "s" + string(rune('0'+s))
		var err error
		for i := 0; i < agentDeliversPerSession && err == nil; i++ {
			n++
			_, err = store.Link(Link{Ref: GitHubRef("acme/api", int64(n)), Workspace: "w1", Session: session, Role: RoleDelivers, Source: SourceCLI})
		}
		if err != nil {
			if !errors.Is(err, ErrTooManyDelivered) {
				t.Fatal(err)
			}
			break
		}
	}
	if got := len(store.Links(Filter{Workspace: "w1", Roles: []string{RoleDelivers}})); got != agentDeliversPerWorkspace {
		t.Fatalf("the workspace's bound: %d delivered", got)
	}
	// The person and the branch detector are not bounded by what agents did.
	if _, err := store.Link(Link{Ref: GitHubRef("acme/api", 5000), Workspace: "w1", Role: RoleDelivers, Source: SourceBranch}); err != nil {
		t.Fatalf("branch: %v", err)
	}
	if _, err := store.Link(Link{Ref: GitHubRef("acme/api", 5001), Workspace: "w1", Role: RoleDelivers, Source: SourcePerson}); err != nil {
		t.Fatalf("person: %v", err)
	}
	// Another workspace has its own bound.
	if _, err := store.Link(Link{Ref: GitHubRef("acme/api", 1), Workspace: "w2", Session: "t1", Role: RoleDelivers, Source: SourceCLI}); err != nil {
		t.Fatalf("another workspace: %v", err)
	}
}

// Links are trimmed per workspace: however many one workspace gets, another
// workspace's links stay.
func TestLinksAreTrimmedPerWorkspace(t *testing.T) {
	store := openStore(t)
	if _, err := store.Link(Link{Ref: GitHubRef("acme/web", 1), Workspace: "quiet", Role: RoleDelivers, Source: SourcePerson}); err != nil {
		t.Fatal(err)
	}
	for i := 1; i <= linksPerWorkspace+20; i++ {
		if _, err := store.Link(Link{Ref: GitHubRef("acme/api", int64(i)), Workspace: "busy", Role: RoleDelivers, Source: SourceBranch}); err != nil {
			t.Fatal(err)
		}
	}
	if got := len(store.Links(Filter{Workspace: "busy"})); got != linksPerWorkspace {
		t.Fatalf("busy keeps %d links", got)
	}
	if got := len(store.Links(Filter{Workspace: "quiet"})); got != 1 {
		t.Fatal("another workspace's link was pushed out")
	}
}

// A read that began before the kept snapshot was observed is stale: two
// reads can finish out of order, and the older one never replaces the newer
// or replays its transitions.
func TestStaleReadIsDropped(t *testing.T) {
	store := openStore(t)
	ref := GitHubRef("acme/api", 7)
	if _, err := store.Link(Link{Ref: ref, Workspace: "w1", Role: RoleDelivers, Source: SourcePerson}); err != nil {
		t.Fatal(err)
	}
	older, newer := time.Now().Add(-time.Minute), time.Now()
	failing := pr("open", "failure", "", "clean", "b", "lint")
	failing.Ref = ref
	if _, err := store.Observe(failing, "", newer); err != nil {
		t.Fatal(err)
	}
	green := pr("open", "pending", "", "clean", "a")
	green.Ref = ref
	events, err := store.Observe(green, "", older)
	if err != nil || len(events) != 0 {
		t.Fatalf("a stale read emitted %v (%v)", names(events), err)
	}
	if kept, _ := store.Item(ref); kept.PR.HeadSHA != "b" || !kept.ObservedAt.Equal(newer) {
		t.Fatalf("the stale read replaced the newer snapshot: %+v", kept)
	}
	// A read that began later is kept.
	merged := pr("merged", "success", "", "", "b")
	merged.Ref = ref
	if events, _ = store.Observe(merged, "", newer.Add(time.Second)); len(events) == 0 {
		t.Fatal("a newer read was dropped")
	}
}
