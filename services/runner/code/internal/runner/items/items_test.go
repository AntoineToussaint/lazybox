package items

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
)

// What `gh pr create` prints, progress on stderr first and the new pull
// request's URL last, as an agent's Bash tool returns it.
const ghPRCreate = `Warning: 1 uncommitted change

Creating pull request for antoine/fix-refund into main in acme/api

https://github.com/acme/api/pull/812
`

const gitPush = `remote:
remote: Create a pull request for 'antoine/fix-refund' on GitHub by visiting:
remote:      https://github.com/acme/api/pull/new/antoine/fix-refund
remote:
To github.com:acme/api.git
 * [new branch]      antoine/fix-refund -> antoine/fix-refund
`

func TestDetectorsTellDeliveredFromMentioned(t *testing.T) {
	for _, cmd := range []string{"git push -u origin HEAD && gh pr create --fill", "lazybox run -- hub pull-request -m fix", "glab mr create --fill"} {
		if !createsPullRequestCommand(cmd) {
			t.Fatalf("missed create: %s", cmd)
		}
	}
	for _, cmd := range []string{"gh pr view 812", "git push", "ngh pr create", "gh pr create --help && gh pr view 1216 --json url --jq .url", "printf '%s\\n' 'gh pr create' 'https://github.com/acme/api/pull/7'", "gh pr create --help", "gh pr create --dry-run", "gh pr create --web", "gh pr create && gh pr view 1216", "true || gh pr create", "gh pr create | cat", "exit 0; gh pr create", "echo gh pr create", "cat <<'EOF'\ngh pr create\nEOF"} {
		if createsPullRequestCommand(cmd) {
			t.Fatalf("read-only or ambiguous command delivered: %s", cmd)
		}
	}
	if ToolCreatesPullRequest("Bash", []byte(`{"command":"echo hello","description":"gh pr create"}`)) || ToolCreatesPullRequest("Read", []byte(`{"command":"gh pr create"}`)) {
		t.Fatal("unrelated tool or description treated as executable input")
	}
	if !ToolCreatesPullRequest("mcp__github__create_pull_request", []byte("{}")) {
		t.Fatal("GitHub create tool")
	}
	if got := Scan("mentions https://github.com/acme/api/pull/812 in prose", true); len(got) != 1 || got[0].Role != RoleMentions {
		t.Fatal(got)
	}
	found := Scan(ghPRCreate, true)
	if len(found) != 1 || found[0].Role != RoleDelivers || found[0].Ref != (Ref{Provider: "github", Key: "acme/api#812"}) || found[0].URL != "https://github.com/acme/api/pull/812" {
		t.Fatalf("gh pr create: %+v", found)
	}
	// git push's hint is not a pull request.
	if found = Scan(gitPush, true); len(found) != 0 {
		t.Fatalf("git push hint: %+v", found)
	}
	// Another command's URLs are mentions; an issue is never delivered.
	found = Scan("See https://github.com/acme/api/pull/7/files and https://github.com/Acme/API/issues/9#issuecomment-1", false)
	if len(found) != 2 || found[0].Role != RoleMentions || found[1].Kind != KindIssue || found[1].Ref.Key != "acme/api#9" {
		t.Fatalf("mentions: %+v", found)
	}
	// A creating command that also printed an older pull request delivers the last one only.
	found = Scan("Based on https://github.com/acme/api/pull/7\nhttps://github.com/acme/api/pull/8\n", true)
	if len(found) != 2 || found[0].Ref.Key != "acme/api#8" || found[0].Role != RoleDelivers || found[1].Role != RoleMentions {
		t.Fatalf("last URL delivered: %+v", found)
	}
	// JSON tool output escapes slashes.
	if found = Scan(`{"html_url":"https:\/\/github.com\/acme\/api\/pull\/5"}`, true); len(found) != 1 || found[0].Role != RoleDelivers || found[0].Ref.Key != "acme/api#5" {
		t.Fatalf("escaped: %+v", found)
	}
	if found = Scan(`{"isError":true,"content":[{"type":"text","text":"https://github.com/acme/api/pull/5"}]}`, true); len(found) != 1 || found[0].Role != RoleMentions {
		t.Fatalf("failed MCP result: %+v", found)
	}
	for _, result := range []string{
		`[{"type":"text","text":"Creating pull request\nhttps://github.com/acme/api/pull/5\n"}]`,
		`[{"type":"text","text":"{\"url\":\"https://github.com/acme/api/pull/5\"}"}]`,
		`{"content":[{"type":"text","text":"{\"html_url\":\"https://github.com/acme/api/pull/5\"}"}]}`,
	} {
		if found = Scan(result, true); len(found) != 1 || found[0].Role != RoleDelivers {
			t.Fatalf("MCP result: %+v", found)
		}
	}
	many := strings.Repeat("https://github.com/acme/api/pull/1 ", 1)
	for i := 2; i < 20; i++ {
		many += "https://github.com/acme/api/pull/" + itoa(int64(i)) + " "
	}
	if found = Scan(many, false); len(found) != maxFound {
		t.Fatalf("a listing contributes at most %d: %d", maxFound, len(found))
	}
	if r, ok := ResolveGitHub("https://github.com/acme/api/pull/812/checks?x=1"); !ok || r.Number != 812 || r.Repository != "acme/api" {
		t.Fatalf("resolve: %+v", r)
	}
	if _, ok := ResolveGitHub("https://gitlab.com/acme/api/-/merge_requests/3"); ok {
		t.Fatal("not a GitHub URL")
	}
}

// What agents really run and print (here acme/platform#1121): the command
// after a heredoc on its own line, the push's hint before the new URL, and
// `lazybox run`'s view.
func TestDetectorsReadRealPullRequestCreation(t *testing.T) {
	input, _ := json.Marshal(map[string]any{"command": "cat > /tmp/pr-body.md <<'EOF'\n## Summary\nEOF\ngh pr create --repo acme/platform --base main --title x --body-file /tmp/pr-body.md", "description": "Open the pull request"})
	if !ToolCreatesPullRequest("Bash", input) {
		t.Fatal("gh pr create on its own line after a heredoc")
	}
	if ToolCreatesPullRequest("Bash", []byte(`{"command":"echo done\ngh pr view 5"}`)) {
		t.Fatal("gh pr view")
	}
	for name, output := range map[string]string{
		"push and create": "remote:      https://github.com/acme/platform/pull/new/feature/quietly-brave-otter        \nremote: \nhttps://github.com/acme/platform/pull/1121",
		"progress first":  "Warning: 1 uncommitted change\n\nCreating pull request for feature/quietly-brave-otter into main in acme/platform\n\nhttps://github.com/acme/platform/pull/1121\n",
		"lazybox run":     "[lazybox] gh pr create --repo acme/platform --fill → exit 0 (2.3s)\nCreating pull request for feature/quietly-brave-otter into main in acme/platform\n\nhttps://github.com/acme/platform/pull/1121\n",
	} {
		found := Scan(output, true)
		if len(found) != 1 || found[0].Role != RoleDelivers || found[0].Ref.Key != "acme/platform#1121" {
			t.Errorf("%s: %+v", name, found)
		}
	}
}

func pr(state, checks, review, mergeable, head string, failing ...string) Item {
	item := Item{Ref: GitHubRef("acme/api", 812), Kind: KindPullRequest, Title: "Fix rounding", Repository: "acme/api", Number: 812, RawState: state,
		PR: &PullRequest{Head: "fix", Base: "main", HeadSHA: head, Checks: checks, Review: review, MergeableState: mergeable}}
	for _, name := range failing {
		item.PR.CheckRuns = append(item.PR.CheckRuns, Check{Name: name, Status: "completed", Conclusion: "failure"})
	}
	item.PR.CheckRuns = append(item.PR.CheckRuns, Check{Name: "build", Status: "completed", Conclusion: "success"})
	return item
}

func names(events []Event) []string {
	var out []string
	for _, e := range events {
		out = append(out, e.Name)
	}
	return out
}

func TestTransitionsAreEmittedOnce(t *testing.T) {
	open := pr("open", "pending", "", "clean", "a")
	if got := names(Transitions(nil, open)); !slices.Equal(got, []string{EventOpened}) {
		t.Fatalf("first sight: %v", got)
	}
	// A first sight already failing says so; one already approved and green is a baseline.
	if got := names(Transitions(nil, pr("open", "failure", "changes_requested", "dirty", "a", "lint"))); !slices.Equal(got, []string{EventOpened, EventChecksFailed, EventChangesRequested, EventConflicts}) {
		t.Fatalf("first sight needing the person: %v", got)
	}
	if got := names(Transitions(nil, pr("open", "success", "approved", "clean", "a"))); !slices.Equal(got, []string{EventOpened}) {
		t.Fatalf("first sight green: %v", got)
	}
	if got := Transitions(nil, pr("merged", "success", "", "", "a")); len(got) != 0 {
		t.Fatalf("first seen merged is history: %v", got)
	}
	failing := pr("open", "failure", "", "clean", "a", "lint", "unit-api")
	events := Transitions(&open, failing)
	if got := names(events); !slices.Equal(got, []string{EventChecksFailed}) || events[0].Reason != "checks failed: lint, unit-api (2 of 3)" || !slices.Equal(events[0].Failed, []string{"lint", "unit-api"}) {
		t.Fatalf("checks failed: %+v", events)
	}
	// The same state again: nothing, however often it is read.
	if got := Transitions(&failing, failing); len(got) != 0 {
		t.Fatalf("no transition: %v", got)
	}
	// A new head failing again is a new failure.
	if got := names(Transitions(&failing, pr("open", "failure", "", "clean", "b", "lint"))); !slices.Equal(got, []string{EventChecksFailed}) {
		t.Fatalf("new head: %v", got)
	}
	green := pr("open", "success", "changes_requested", "clean", "b")
	if got := names(Transitions(&failing, green)); !slices.Equal(got, []string{EventChecksPassed, EventChangesRequested}) {
		t.Fatalf("passed and review: %v", got)
	}
	approved := pr("open", "success", "approved", "dirty", "b")
	if got := names(Transitions(&green, approved)); !slices.Equal(got, []string{EventApproved, EventConflicts}) {
		t.Fatalf("approved, conflicts: %v", got)
	}
	if got := names(Transitions(&approved, pr("merged", "success", "approved", "", "b"))); !slices.Equal(got, []string{EventMerged}) {
		t.Fatalf("merged: %v", got)
	}
	if got := names(Transitions(&open, pr("closed", "pending", "", "", "a"))); !slices.Equal(got, []string{EventClosed}) {
		t.Fatalf("closed: %v", got)
	}
}

func TestStorePersistsLinksAndPlacesEvents(t *testing.T) {
	path := filepath.Join(t.TempDir(), "items", "items.json")
	store, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	ref := GitHubRef("acme/api", 812)
	if added, err := store.Link(Link{Ref: ref, Workspace: "w1", Role: RoleMentions, Source: SourceAgentOutput}); err != nil || !added {
		t.Fatalf("mention: %v %v", added, err)
	}
	// A mention becomes delivers when something says the workspace delivered it.
	if added, _ := store.Link(Link{Ref: ref, Workspace: "w1", Session: "s1", Conversation: "c1", Role: RoleDelivers, Source: SourceCLI}); !added {
		t.Fatal("upgrade to delivers")
	}
	if added, _ := store.Link(Link{Ref: ref, Workspace: "w1", Role: RoleMentions, Source: SourceAgentOutput}); added {
		t.Fatal("a delivered item is not demoted to a mention")
	}
	if _, err = store.Link(Link{Ref: ref, Workspace: "w1", Role: "owns"}); err == nil {
		t.Fatal("unknown role")
	}
	events, err := store.Observe(pr("open", "pending", "", "clean", "a"), `{"pull":"x"}`, time.Time{})
	if err != nil || len(events) != 1 || events[0].Name != EventOpened || events[0].Workspace != "w1" || events[0].Session != "s1" || events[0].ID == "" {
		t.Fatalf("opened, placed in the delivering conversation: %+v %v", events, err)
	}
	if events, _ = store.Observe(pr("open", "pending", "", "clean", "a"), "", time.Time{}); len(events) != 0 {
		t.Fatalf("an unchanged read emits nothing: %+v", events)
	}
	if _, err = store.SeeBranch(Branch{Workspace: "w1", Member: "api", Repository: "acme/api", Name: "fix", Pushed: true}); err != nil {
		t.Fatal(err)
	}

	// A restart loses nothing: links, snapshot, validator, branch and events.
	again, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	links := again.Links(Filter{Workspace: "w1"})
	if len(links) != 1 || links[0].Role != RoleDelivers || links[0].Item.Title != "Fix rounding" || links[0].Session != "s1" {
		t.Fatalf("after restart: %+v", links)
	}
	tracked := again.Tracked()
	if len(tracked) != 1 || tracked[0].Tracking.ETag != `{"pull":"x"}` || !tracked[0].Known {
		t.Fatalf("tracking after restart: %+v", tracked)
	}
	if len(again.Branches()) != 1 || len(again.Events("w1", 10)) != 1 {
		t.Fatalf("branches %+v events %+v", again.Branches(), again.Events("w1", 10))
	}
	// After a restart the next read compares with the persisted snapshot:
	// what happened while the Runner was down is emitted once.
	if events, _ = again.Observe(pr("open", "failure", "", "clean", "a", "lint"), "", time.Time{}); len(events) != 1 || events[0].Name != EventChecksFailed {
		t.Fatalf("after restart: %+v", events)
	}
	if item, ok := again.BranchPullRequest("w1", "ACME/api", "fix"); !ok || item.Number != 812 {
		t.Fatalf("branch pull request: %+v %v", item, ok)
	}

	// A deleted workspace keeps what it delivered, as history; its mentions go.
	if _, err = again.Link(Link{Ref: GitHubRef("acme/api", 9), Workspace: "w1", Role: RoleMentions}); err != nil {
		t.Fatal(err)
	}
	if err = again.WorkspaceGone("w1", "Payments"); err != nil {
		t.Fatal(err)
	}
	links = again.Links(Filter{})
	if len(links) != 1 || !links[0].Deleted || links[0].WorkspaceName != "Payments" || len(again.Links(Filter{Live: true})) != 0 || len(again.Branches()) != 0 {
		t.Fatalf("deleted workspace: %+v", links)
	}

	// A corrupt file is kept aside, not overwritten.
	if err = os.WriteFile(path, []byte("{"), 0o600); err != nil {
		t.Fatal(err)
	}
	if empty, err := Open(path); err != nil || len(empty.Links(Filter{})) != 0 {
		t.Fatalf("corrupt: %v", err)
	}
	if _, err = os.Stat(path + ".corrupt"); err != nil {
		t.Fatal("the corrupt file is kept")
	}
}

func TestMentionsAreBounded(t *testing.T) {
	store, err := Open(filepath.Join(t.TempDir(), "items.json"))
	if err != nil {
		t.Fatal(err)
	}
	for i := 1; i <= mentionsPerWorkspace+10; i++ {
		if _, err = store.Link(Link{Ref: GitHubRef("acme/api", int64(i)), Workspace: "w1", Role: RoleMentions}); err != nil {
			t.Fatal(err)
		}
	}
	links := store.Links(Filter{Workspace: "w1"})
	if len(links) != mentionsPerWorkspace || slices.ContainsFunc(links, func(l LinkView) bool { return l.Ref.Key == "acme/api#1" }) {
		t.Fatalf("the oldest mentions go first: %d", len(links))
	}
	if len(store.Tracked()) != 0 {
		t.Fatal("mentions are never tracked")
	}
}

func TestScheduleFollowsFocusStateAndFailures(t *testing.T) {
	now := time.Date(2026, 11, 17, 12, 0, 0, 0, time.UTC)
	open := TrackedItem{Known: true, Item: Item{State: StateReview}, Tracking: Tracking{CheckedAt: now.Add(-90 * time.Second)}}
	if due, _ := Due(open, now, true); !due {
		t.Fatal("open, focused: every minute")
	}
	if due, next := Due(open, now, false); due || !next.Equal(now.Add(-90*time.Second+OpenUnfocused)) {
		t.Fatal("open, unfocused: every five minutes")
	}
	open.Tracking.Failures = 3
	if due, _ := Due(open, now, true); due {
		t.Fatal("failures back off")
	}
	merged := TrackedItem{Known: true, Item: Item{State: StateMerged, ClosedAt: now.Add(-2 * 24 * time.Hour)}, Tracking: Tracking{CheckedAt: now.Add(-23 * time.Hour)}}
	if due, _ := Due(merged, now, true); due {
		t.Fatal("ended: once a day")
	}
	merged.Tracking.CheckedAt = now.Add(-25 * time.Hour)
	if due, _ := Due(merged, now, true); !due {
		t.Fatal("ended, a day later")
	}
	merged.Item.ClosedAt = now.Add(-8 * 24 * time.Hour)
	if due, next := Due(merged, now, true); due || !next.IsZero() {
		t.Fatal("a week after it ended, never again")
	}
	if due, _ := Due(TrackedItem{}, now, false); !due {
		t.Fatal("never read: now")
	}
	branch := Branch{Pushed: true, LastSeen: now.Add(-time.Hour)}
	if !BranchDue(branch, now, false) {
		t.Fatal("a pushed branch never asked about")
	}
	branch.CheckedAt = now.Add(-30 * time.Second)
	if BranchDue(branch, now, true) || BranchDue(Branch{Pushed: true, LastSeen: now.Add(-8 * 24 * time.Hour)}, now, true) {
		t.Fatal("asked recently, or gone a week: not due")
	}
	// A branch not known to be pushed (an agent's own push to a bare
	// source leaves no remote-tracking ref) is asked about every five
	// minutes, focused or not.
	unknown := Branch{LastSeen: now}
	if !BranchDue(unknown, now, true) {
		t.Fatal("a branch not known to be pushed, never asked about")
	}
	unknown.CheckedAt = now.Add(-2 * time.Minute)
	if BranchDue(unknown, now, true) {
		t.Fatal("not known to be pushed: not every minute")
	}
	unknown.CheckedAt = now.Add(-OpenUnfocused)
	if !BranchDue(unknown, now, true) {
		t.Fatal("not known to be pushed: every five minutes")
	}
	unknown.Failures = 2
	if BranchDue(unknown, now, true) {
		t.Fatal("failed lookups back off")
	}
	if backoff(time.Minute, 10) != backoffMax || backoff(EndedInterval, 3) != EndedInterval {
		t.Fatal("backoff caps")
	}
}

// What a GitHub Actions job's log looks like: the checkout step, then a
// failing `go test` step, each line stamped.
const goTestJob = "2026-11-17T10:00:00.1000000Z ##[group]Run actions/checkout@v4\n" +
	"2026-11-17T10:00:00.1000000Z with:\n" +
	"2026-11-17T10:00:00.1000000Z   repository: acme/api\n" +
	"2026-11-17T10:00:00.1000000Z ##[endgroup]\n" +
	"2026-11-17T10:00:01.0000000Z Syncing repository: acme/api\n" +
	"2026-11-17T10:00:02.0000000Z ##[group]Run go test ./...\n" +
	"2026-11-17T10:00:02.0000000Z go test ./...\n" +
	"2026-11-17T10:00:02.0000000Z shell: /usr/bin/bash -e {0}\n" +
	"2026-11-17T10:00:02.0000000Z ##[endgroup]\n" +
	"2026-11-17T10:00:03.0000000Z ok  \tacme/api/ledger\t0.012s\n" +
	"2026-11-17T10:00:03.0000000Z ok  \tacme/api/store\t0.020s\n" +
	"2026-11-17T10:00:04.0000000Z --- FAIL: TestRefundRounding (0.00s)\n" +
	"2026-11-17T10:00:04.0000000Z     refund_test.go:42: got 10.01, want 10.00\n" +
	"2026-11-17T10:00:04.0000000Z FAIL\n" +
	"2026-11-17T10:00:04.0000000Z FAIL\tacme/api/refund\t0.015s\n" +
	"2026-11-17T10:00:04.0000000Z FAIL\n" +
	"2026-11-17T10:00:04.0000000Z ##[error]Process completed with exit code 1.\n" +
	"2026-11-17T10:00:05.0000000Z Post job cleanup.\n" +
	"2026-11-17T10:00:05.0000000Z [command]/usr/bin/git version\n"

func TestCompactJobLogKeepsTheFailingStep(t *testing.T) {
	view := CompactJobLog([]byte(goTestJob))
	if view.Step != "go test ./..." || view.Exit != 1 || view.Filter != "go-test" {
		t.Fatalf("failing step and filter: %+v", view)
	}
	for _, want := range []string{"TestRefundRounding", "refund_test.go:42: got 10.01, want 10.00"} {
		if !strings.Contains(view.Text, want) {
			t.Fatalf("view lost %q:\n%s", want, view.Text)
		}
	}
	for _, gone := range []string{"2026-11-17T", "Syncing repository", "repository: acme/api", "##[", "git version", "acme/api/ledger"} {
		if strings.Contains(view.Text, gone) {
			t.Fatalf("view kept %q:\n%s", gone, view.Text)
		}
	}
	// A log with no failing step marker is compacted whole.
	if whole := CompactJobLog([]byte("2026-11-17T10:00:00Z boom\n")); whole.Step != "" || !strings.Contains(whole.Text, "boom") {
		t.Fatalf("whole: %+v", whole)
	}
	if ActionsJobID(Check{URL: "https://github.com/acme/api/actions/runs/41/job/77"}) != 77 || ActionsJobID(Check{URL: "https://ci.example/1"}) != 0 {
		t.Fatal("job id from the check's link")
	}
}

// The GitHub provider reads through conditional requests: a second refresh
// of an unchanged pull request comes back unchanged and costs only 304s.
func TestGitHubProviderRefreshUsesValidators(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["acme/api"] = "main"
	fake.Update(func(s *githubtest.Server) {
		s.Pulls["acme/api"] = []*githubtest.PR{{Number: 812, Title: "Fix rounding", Head: "fix", Base: "main", HeadSHA: "abc", State: "open", MergeableState: "clean"}}
		s.Checks["abc"] = []githubtest.Check{{ID: 5, Name: "test", Status: "completed", Conclusion: "failure", URL: "https://github.com/acme/api/actions/runs/1/job/5"}}
	})
	provider := GitHub{API: github.NewAt(fake.URL), Credential: func(_ context.Context, use func(string) error) error { return use("ghp_fixture") }}
	ref := GitHubRef("acme/api", 812)
	first, err := provider.Refresh(ctx, []Item{{Ref: ref}}, map[string]string{})
	if err != nil || len(first.Items) != 1 || first.ETags[ref.ID()] == "" {
		t.Fatalf("first: %+v %v", first, err)
	}
	item := first.Items[0]
	if item.State != StateReview || item.PR.Checks != "failure" || item.PR.Head != "fix" || len(item.PR.Failing()) != 1 || item.PR.CheckRuns[0].ID != 5 {
		t.Fatalf("snapshot: %+v %+v", item, item.PR)
	}
	second, err := provider.Refresh(ctx, []Item{item}, first.ETags)
	if err != nil || len(second.Items) != 0 || len(second.Unchanged) != 1 {
		t.Fatalf("second: %+v %v", second, err)
	}
	fake.Update(func(s *githubtest.Server) {
		s.Pulls["acme/api"][0].Merged, s.Pulls["acme/api"][0].State = true, "closed"
	})
	third, err := provider.Refresh(ctx, []Item{item}, first.ETags)
	if err != nil || len(third.Items) != 1 || third.Items[0].State != StateMerged || third.Items[0].ClosedAt.IsZero() {
		t.Fatalf("merged: %+v %v", third, err)
	}
	fake.Update(func(s *githubtest.Server) { s.RateLimited = true })
	if _, err = provider.Refresh(ctx, []Item{item}, first.ETags); err == nil {
		t.Fatal("a rate limit stops the refresh")
	} else if limited, ok := AsRateLimited(err); !ok || limited.Until.IsZero() {
		t.Fatalf("rate limit: %v", err)
	}
	found, tag, _, err := provider.PullRequestsFor(ctx, "acme/api", "fix", "")
	if err == nil || tag != "" || found != nil {
		t.Fatalf("rate limited branch lookup: %v", err)
	}
}
