package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
)

// Tracked pull requests (GITHUB.md "Tracked pull requests",
// PROJECTS_AND_INTEGRATIONS.md). A workspace delivers the pull requests its
// agents and the person open; four detectors link them — the PR loop's
// Create PR (person), any branch a workspace repository has been on
// (branch), what an agent's tools print (agent-output) and `lazybox link`
// (cli) — and a background pass keeps the linked ones fresh through their
// provider, with conditional requests. Each transition is emitted once, to
// hooks, the delivering conversation's journal, and push.

// GitHubItemsAPI is what tracking and its reactions read from GitHub
// (github.Client); a test points it at a fake.
type GitHubItemsAPI interface {
	workitems.GitHubAPI
	JobLog(ctx context.Context, token, fullName string, jobID int64) ([]byte, error)
	ReviewComments(ctx context.Context, token, fullName string, number int64) ([]github.ReviewComment, []github.Review, error)
	PullRequestByNumber(ctx context.Context, token, fullName string, number int64) (github.PullRequestStatus, error)
}

type itemsState struct {
	store *workitems.Store
	// focusedAt is when a focused window last asked (Unix ms): open pull
	// requests are read every minute while one has focus, else every five.
	focusedAt atomic.Int64
	wake      chan struct{}
	// pass serializes tracking passes.
	pass sync.Mutex

	mu          sync.Mutex
	pausedUntil time.Time
	lastError   string
	branchesAt  time.Time
	subscribers []func(workitems.Event)
	// tools remembers, per execution, which running tool calls open a pull
	// request: a provider reports a call's input and its result apart.
	tools   map[string]map[string]bool
	origins map[string]originEntry
	// linked: per execution, when its agent's recent `lazybox link` calls
	// were (agentLinkRate).
	linked map[string][]time.Time
}

type originEntry struct {
	github string
	at     time.Time
}

const (
	// itemPoll is how often the tracking pass looks for due items.
	itemPoll = 10 * time.Second
	// focusWindow: a window that asked with focus this recently counts as focused.
	focusWindow = 2 * time.Minute
	// branchReadEvery is how often the pass reads each worktree's branch.
	branchReadEvery = time.Minute
	originFresh     = 10 * time.Minute
	refreshBatch    = 25
	// itemPassTimeout bounds one tracking pass.
	itemPassTimeout = 2 * time.Minute
	// toolCallsKept bounds the running tool calls remembered per execution.
	toolCallsKept = 64
	// agentLinksPerMinute bounds an agent's `lazybox link` calls.
	agentLinksPerMinute = 10
)

// ErrLinkRate refuses an agent's link past agentLinksPerMinute.
var ErrLinkRate = fmt.Errorf("this conversation linked %d items in the last minute; wait before linking more", agentLinksPerMinute)

// ErrNotDelivered is a pull request this workspace does not deliver.
var ErrNotDelivered = errors.New("this workspace does not deliver that pull request")

const connectForTracking = "Connect GitHub in Settings → Repositories to track pull requests"

func (b *Bench) openItems() error {
	store, err := workitems.Open(filepath.Join(b.Config.State, "items", "items.json"))
	if err != nil {
		return err
	}
	b.workItems.store = store
	b.workItems.wake = make(chan struct{}, 1)
	b.workItems.tools = map[string]map[string]bool{}
	b.workItems.origins = map[string]originEntry{}
	b.workItems.linked = map[string][]time.Time{}
	every := b.Config.ItemPoll
	if every == 0 {
		every = itemPoll
	}
	if every < 0 {
		return nil // tests drive TrackItems themselves
	}
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		// What agents opened while no detector ran (items_backfill.go).
		ctx, cancel := context.WithTimeout(b.ctx, backfillTimeout)
		b.BackfillItems(ctx)
		cancel()
		ticker := time.NewTicker(every)
		defer ticker.Stop()
		for {
			select {
			case <-b.ctx.Done():
				return
			case <-ticker.C:
			case <-b.workItems.wake:
			}
			b.workItems.mu.Lock()
			readBranches := time.Since(b.workItems.branchesAt) >= branchReadEvery
			if readBranches {
				b.workItems.branchesAt = time.Now()
			}
			b.workItems.mu.Unlock()
			ctx, cancel := context.WithTimeout(b.ctx, itemPassTimeout)
			b.trackItems(ctx, readBranches)
			cancel()
		}
	}()
	return nil
}

// WorkItems is the store of links and snapshots.
func (b *Bench) WorkItems() *workitems.Store { return b.workItems.store }

// wakeItems asks for a tracking pass soon (a new link, a newly pushed branch).
func (b *Bench) wakeItems() {
	select {
	case b.workItems.wake <- struct{}{}:
	default:
	}
}

// ItemsFocused records that a window with focus asked for tracked items.
func (b *Bench) ItemsFocused() {
	b.workItems.focusedAt.Store(time.Now().UnixMilli())
}

func (b *Bench) itemsFocused(now time.Time) bool {
	at := b.workItems.focusedAt.Load()
	return at > 0 && now.Sub(time.UnixMilli(at)) < focusWindow
}

// ItemTrackingStatus is how often open pull requests are read now, and why
// tracking cannot read what a workspace delivers (every workspace's when
// workspace is empty), if it cannot: the provider as a whole (not
// connected, rate limited), else the last lookup or read of it that failed.
func (b *Bench) ItemTrackingStatus(workspace string) (time.Duration, string) {
	t := &b.workItems
	now := time.Now()
	t.mu.Lock()
	reason := t.lastError
	t.mu.Unlock()
	if reason == "" && !b.githubTrackable() {
		reason = connectForTracking
	}
	if reason == "" && t.store != nil {
		reason = t.store.Problem(workspace, now)
	}
	return workitems.Interval(b.itemsFocused(now)), reason
}

// SubscribeItemEvents calls fn with every transition (push notifications).
func (b *Bench) SubscribeItemEvents(fn func(workitems.Event)) {
	b.workItems.mu.Lock()
	b.workItems.subscribers = append(b.workItems.subscribers, fn)
	b.workItems.mu.Unlock()
}

func (b *Bench) githubItems() GitHubItemsAPI {
	if b.GitHubItems != nil {
		return b.GitHubItems
	}
	api, _ := b.GitHubPulls.(GitHubItemsAPI)
	return api
}

func (b *Bench) githubTrackable() bool {
	return b.GitHub != nil && b.GitHub.Status().State == "connected" && b.githubItems() != nil
}

func (b *Bench) githubCredential(ctx context.Context, use func(token string) error) error {
	if !b.githubTrackable() {
		return errors.New(connectForTracking)
	}
	return b.GitHub.WithCredential(ctx, use)
}

// provider is the built-in GitHub provider over the Runner's connection.
func (b *Bench) itemProvider() workitems.GitHub {
	return workitems.GitHub{API: b.githubItems(), Credential: b.githubCredential}
}

func (b *Bench) setTrackingError(reason string) {
	b.workItems.mu.Lock()
	b.workItems.lastError = reason
	b.workItems.mu.Unlock()
}

// TrackItems runs one tracking pass: it reads the worktrees' branches, asks
// about pushed branches that may have pull requests, and reads every linked
// item that is due. The Runner runs one every few seconds (reading branches
// once a minute); tests call it.
func (b *Bench) TrackItems(ctx context.Context) { b.trackItems(ctx, true) }

func (b *Bench) trackItems(ctx context.Context, readBranches bool) {
	t := &b.workItems
	if t.store == nil {
		return
	}
	t.pass.Lock()
	defer t.pass.Unlock()
	now := time.Now()
	b.forgetDeletedWorkspaces()
	if !b.githubTrackable() {
		b.setTrackingError(connectForTracking)
		return
	}
	t.mu.Lock()
	paused := now.Before(t.pausedUntil)
	t.mu.Unlock()
	if paused {
		return
	}
	if readBranches {
		b.readWorktreeBranches(ctx)
	}
	focused := b.itemsFocused(now)
	provider := b.itemProvider()
	for _, branch := range t.store.Branches() {
		if !workitems.BranchDue(branch, now, focused) {
			continue
		}
		if err := b.lookUpBranch(ctx, provider, branch); err != nil {
			if b.pauseOn(err) {
				return
			}
		}
	}
	var due []workitems.Item
	etags := map[string]string{}
	for _, tracked := range t.store.Tracked() {
		if tracked.Item.Ref.Provider != provider.Name() {
			continue
		}
		if ok, _ := workitems.Due(tracked, now, focused); ok {
			due = append(due, tracked.Item)
			etags[tracked.Item.Ref.ID()] = tracked.Tracking.ETag
		}
	}
	for start := 0; start < len(due); start += refreshBatch {
		batch := due[start:min(start+refreshBatch, len(due))]
		if err := b.refreshItems(ctx, provider, batch, etags); err != nil {
			if b.pauseOn(err) {
				return
			}
		}
	}
	b.setTrackingError("")
}

// pauseOn waits out a rate limit; true when it paused or the connection failed.
func (b *Bench) pauseOn(err error) bool {
	if limited, ok := workitems.AsRateLimited(err); ok {
		until := limited.Until
		if until.IsZero() || until.Before(time.Now().Add(time.Minute)) {
			until = time.Now().Add(5 * time.Minute)
		}
		b.workItems.mu.Lock()
		b.workItems.pausedUntil = until
		b.workItems.lastError = (&workitems.RateLimited{Until: until}).Error()
		b.workItems.mu.Unlock()
		return true
	}
	if errors.Is(err, github.ErrUnauthorized) {
		b.setTrackingError(err.Error())
		return true
	}
	return false
}

// refreshItems reads a batch and records what changed.
func (b *Bench) refreshItems(ctx context.Context, provider workitems.Provider, batch []workitems.Item, etags map[string]string) error {
	store := b.workItems.store
	// A reaction's read (refreshNow) runs beside the tracking pass: the one
	// that began first and ends last is stale (Observation.ReadAt).
	readAt := store.Now()
	refreshed, err := provider.Refresh(ctx, batch, etags)
	var reads []workitems.Observation
	for _, item := range refreshed.Items {
		reads = append(reads, workitems.Observation{Item: item, ETag: refreshed.ETags[item.Ref.ID()], ReadAt: readAt})
	}
	if len(reads) > 0 {
		events, werr := store.ObserveAll(reads)
		if werr != nil {
			log.Printf("tracked items: %v", werr)
		}
		b.dispatchItemEvents(events)
	}
	for _, ref := range refreshed.Unchanged {
		_ = store.Unchanged(ref)
	}
	for id, cause := range refreshed.Failed {
		for _, item := range batch {
			if item.Ref.ID() == id {
				_ = store.Failed(item.Ref, cause)
			}
		}
	}
	return err
}

// lookUpBranch asks which pull requests come from a pushed branch, and links
// the ones this workspace delivers: those opened since the workspace was
// first seen on the branch (with an hour's grace), and an older one, open or
// not, only when its head commit is on the workspace's branch — its work was
// opened before tracking saw it. Any other older pull request is someone
// else's work on a branch of the same name.
func (b *Bench) lookUpBranch(ctx context.Context, finder workitems.BranchFinder, branch workitems.Branch) error {
	store := b.workItems.store
	found, tag, same, err := finder.PullRequestsFor(ctx, branch.Repository, branch.Name, branch.ETag)
	if err != nil {
		if _, limited := workitems.AsRateLimited(err); !limited && !errors.Is(err, github.ErrUnauthorized) && ctx.Err() == nil {
			// Said in ListLinks until a lookup answers (Store.Problem).
			_ = store.CheckedBranch(branch, "", err)
		}
		return err
	}
	_ = store.CheckedBranch(branch, tag, nil)
	if same {
		return nil
	}
	for _, pr := range found {
		// UpdatedAt is when the pull request was opened (PullRequestsFor).
		if pr.UpdatedAt.Before(branch.FirstSeen.Add(-time.Hour)) && !b.branchHas(ctx, branch, pr.PR.HeadSHA) {
			continue
		}
		session, conversation := b.recentConversation(branch.Workspace)
		b.linkItem(workitems.Link{Ref: pr.Ref, Workspace: branch.Workspace, Member: branch.Member, Session: session, Conversation: conversation, Role: workitems.RoleDelivers, Source: workitems.SourceBranch})
	}
	return nil
}

// branchHas reports whether a commit is on the branch a workspace repository
// was seen on.
func (b *Bench) branchHas(ctx context.Context, branch workitems.Branch, commit string) bool {
	if b.Collections == nil || commit == "" {
		return false
	}
	w, err := b.Collections.Get(branch.Workspace)
	if err != nil {
		return false
	}
	for _, m := range w.Members {
		if m.Name == branch.Member && m.Ready {
			return gitpublish.Git{}.Contains(ctx, m.Path, branch.Name, commit)
		}
	}
	return false
}

// recentConversation is the workspace's most recently active conversation:
// the best guess at who delivered what a branch shows.
func (b *Bench) recentConversation(workspace string) (string, string) {
	var best *Execution
	var bestAt time.Time
	for _, e := range b.List() {
		if e.CollectionID != workspace || agent.IsAssistant(e.Purpose) {
			continue
		}
		s := e.Engine.Snapshot()
		at := e.Saved.CreatedAt
		if root, ok := s.Agents["root"]; ok && root.UpdatedAt.After(at) {
			at = root.UpdatedAt
		}
		if best == nil || at.After(bestAt) {
			best, bestAt = e, at
		}
	}
	if best == nil {
		return "", ""
	}
	return best.ID, conversationOf(best)
}

// readWorktreeBranches records the branch each ready workspace repository is
// on, so a pull request opened from it is found even with no window open.
func (b *Bench) readWorktreeBranches(ctx context.Context) {
	if b.Collections == nil {
		return
	}
	git := gitpublish.Git{}
	for _, w := range b.Collections.List() {
		if w.State == "deleted" {
			continue
		}
		for _, m := range w.Members {
			if !m.Ready || ctx.Err() != nil {
				continue
			}
			repository := b.githubOrigin(ctx, m.Path)
			if repository == "" {
				continue
			}
			tracking, err := git.Track(ctx, m.Path, m.BaseCommit)
			if err != nil || tracking.Branch == "" {
				continue
			}
			// The repository's own default branch (origin/HEAD), not only
			// the usual names: no pull request is looked for from it.
			defaultBranch, _ := git.DefaultBranch(ctx, m.Path)
			b.SeeWorkspaceBranch(w.ID, m.Name, repository, tracking.Branch, tracking.Published, defaultBranch)
		}
	}
}

func (b *Bench) githubOrigin(ctx context.Context, path string) string {
	t := &b.workItems
	t.mu.Lock()
	cached, ok := t.origins[path]
	t.mu.Unlock()
	if ok && time.Since(cached.at) < originFresh {
		return cached.github
	}
	origin, err := gitpublish.Git{}.Origin(ctx, path)
	repository := ""
	if err == nil {
		repository = origin.GitHub
	}
	t.mu.Lock()
	t.origins[path] = originEntry{github: repository, at: time.Now()}
	t.mu.Unlock()
	return repository
}

// defaultBranches are never asked about: a pull request comes from a branch
// of its own.
var defaultBranches = map[string]bool{"main": true, "master": true, "trunk": true, "develop": true}

// SeeWorkspaceBranch records that a workspace repository is on a branch (the
// sidebar's summaries and the tracking pass call it). A branch seen for the
// first time, or newly pushed, is asked about at once.
func (b *Bench) SeeWorkspaceBranch(workspace, member, repository, branch string, pushed bool, defaultBranch string) {
	store := b.workItems.store
	if store == nil || repository == "" || branch == "" || defaultBranches[branch] || branch == strings.TrimPrefix(defaultBranch, "origin/") {
		return
	}
	fresh, err := store.SeeBranch(workitems.Branch{Workspace: workspace, Member: member, Repository: repository, Name: branch, Pushed: pushed})
	if err == nil && fresh && pushed {
		b.wakeItems()
	}
}

// forgetDeletedWorkspaces marks the links of workspaces that are gone.
func (b *Bench) forgetDeletedWorkspaces() {
	if b.Collections == nil {
		return
	}
	seen := map[string]bool{}
	for _, l := range b.workItems.store.Links(workitems.Filter{Live: true}) {
		if seen[l.Workspace] {
			continue
		}
		seen[l.Workspace] = true
		if w, err := b.Collections.Get(l.Workspace); err != nil || w.State == "deleted" {
			_ = b.workItems.store.WorkspaceGone(l.Workspace, firstOf(w.Name, l.WorkspaceName))
		}
	}
}

func (b *Bench) itemsWorkspaceGone(id, name string) {
	if b.workItems.store != nil {
		_ = b.workItems.store.WorkspaceGone(id, name)
	}
}

// linkItem records a link, with the workspace's name and the member whose
// origin is the item's repository, and reads a new delivered item soon. An
// agent's delivers link to a repository no workspace member comes from is
// recorded as a mention: an agent cannot make Lazybox track, react to and
// notify about any repository's pull requests.
func (b *Bench) linkItem(l workitems.Link) (bool, error) {
	store := b.workItems.store
	if store == nil {
		return false, errors.New("item tracking is unavailable")
	}
	if b.Collections != nil {
		if w, err := b.Collections.Get(l.Workspace); err == nil {
			l.WorkspaceName = w.Name
			if l.Member == "" {
				if repository, _, ok := workitems.ParseGitHubKey(l.Ref.Key); ok {
					ctx, cancel := context.WithTimeout(b.ctx, 3*time.Second)
					for _, m := range w.Members {
						if m.Ready && strings.EqualFold(b.githubOrigin(ctx, m.Path), repository) {
							l.Member = m.Name
							break
						}
					}
					cancel()
				}
			}
		}
	}
	if l.Role == workitems.RoleDelivers && workitems.AgentMade(l.Source) && l.Member == "" {
		l.Role = workitems.RoleMentions
	}
	added, err := store.Link(l)
	if err == nil && added && l.Role != workitems.RoleMentions {
		b.wakeItems()
	}
	return added, err
}

// LinkItemURL links what a URL names to a workspace (`lazybox link`, the
// owner's LinkItem). session, when set, is the execution asking: its
// workspace and conversation. role empty is delivers for a pull request,
// mentions otherwise; an issue is never delivered. An agent's links (source
// cli) are rate limited per execution (ErrLinkRate) and bounded
// (workitems.ErrTooManyDelivered).
func (b *Bench) LinkItemURL(session, workspace, url, role, source string) (workitems.Resolved, error) {
	resolved, ok := workitems.ResolveGitHub(url)
	if !ok {
		return resolved, errors.New("not a pull request or issue URL Lazybox recognizes (https://github.com/owner/name/pull/N or /issues/N)")
	}
	if session != "" && workitems.AgentMade(source) && !b.allowAgentLink(session, time.Now()) {
		return resolved, ErrLinkRate
	}
	conversation := ""
	if session != "" {
		e, err := b.Get(session)
		if err != nil {
			return resolved, err
		}
		if e.CollectionID == "" {
			return resolved, errors.New("this conversation is not in a workspace")
		}
		workspace, conversation = e.CollectionID, conversationOf(e)
	}
	if workspace == "" {
		return resolved, errors.New("workspace required")
	}
	if b.Collections != nil {
		if w, err := b.Collections.Get(workspace); err != nil || w.State == "deleted" {
			return resolved, errors.New("workspace not found")
		}
	}
	switch role {
	case "":
		role = workitems.RoleMentions
		if resolved.Kind == workitems.KindPullRequest {
			role = workitems.RoleDelivers
		}
	case workitems.RoleDelivers, workitems.RoleMentions:
	default:
		return resolved, errors.New("--role is delivers or mentions")
	}
	// A delivered item is read as a pull request; an issue is mentioned.
	if role == workitems.RoleDelivers && resolved.Kind != workitems.KindPullRequest {
		return resolved, errors.New("only a pull request can be delivered; link an issue as a mention")
	}
	_, err := b.linkItem(workitems.Link{Ref: resolved.Ref, Workspace: workspace, Session: session, Conversation: conversation, Role: role, Source: source})
	return resolved, err
}

// allowAgentLink counts one of an execution's links, false past
// agentLinksPerMinute in the last minute.
func (b *Bench) allowAgentLink(session string, now time.Time) bool {
	t := &b.workItems
	t.mu.Lock()
	defer t.mu.Unlock()
	recent := slices.DeleteFunc(t.linked[session], func(at time.Time) bool { return now.Sub(at) >= time.Minute })
	if len(recent) >= agentLinksPerMinute {
		t.linked[session] = recent
		return false
	}
	t.linked[session] = append(recent, now)
	// Forget executions that stopped linking, so the map stays small.
	if len(t.linked) > 256 {
		for id, at := range t.linked {
			if len(at) == 0 || now.Sub(at[len(at)-1]) >= time.Minute {
				delete(t.linked, id)
			}
		}
	}
	return true
}

// ItemReadStart is when a read of tracked items begins, on the item store's
// clock: what ObservePullRequestLoop's readAt takes.
func (b *Bench) ItemReadStart() time.Time {
	if b.workItems.store == nil {
		return time.Now()
	}
	return b.workItems.store.Now()
}

// ObservePullRequestLoop records what the PR loop read or created about a
// member's branch: the workspace delivers it (person for Create PR, else
// branch), and the read is a fresh snapshot, whose transitions are emitted.
// readAt is when the read began (ItemReadStart; zero: not checked): a read
// that began before the kept snapshot was observed is dropped.
func (b *Bench) ObservePullRequestLoop(workspace, member, repository string, status github.PullRequestStatus, created bool, readAt time.Time) {
	store := b.workItems.store
	if store == nil || !status.Found || status.Number == 0 || repository == "" {
		return
	}
	item := workitems.ItemOfStatus(repository, status, time.Time{})
	if old, ok := store.Item(item.Ref); ok && !old.ClosedAt.IsZero() && workitems.Ended(item.State) {
		item.ClosedAt = old.ClosedAt
	}
	source := workitems.SourceBranch
	if created {
		source = workitems.SourcePerson
	}
	// A link that exists keeps its conversation; a new one goes to the
	// conversation at work in the workspace.
	link := workitems.Link{Ref: item.Ref, Workspace: workspace, Member: member, Role: workitems.RoleDelivers, Source: source}
	link.Session, link.Conversation = b.recentConversation(workspace)
	if _, err := b.linkItem(link); err != nil {
		return
	}
	events, err := store.Observe(item, "", readAt)
	if err != nil {
		log.Printf("tracked items: %v", err)
	}
	b.dispatchItemEvents(events)
}

// itemToolEvent reads one tool call or result of an agent for the pull
// requests it names (engine TurnHooks.Tool): a command that opens one
// delivers the URL it printed; any other recognized URL is a mention.
func (b *Bench) itemToolEvent(e *Execution, ev agent.Event) {
	if b.workItems.store == nil || e.CollectionID == "" {
		return
	}
	call, ok := toolCallOf(ev)
	if !ok {
		return
	}
	t := &b.workItems
	creates := call.creates()
	key := ev.AgentID + "\x00" + call.ID
	t.mu.Lock()
	calls := t.tools[e.ID]
	if calls == nil {
		calls = map[string]bool{}
		t.tools[e.ID] = calls
	}
	if creates && call.ID != "" {
		if len(calls) >= toolCallsKept {
			clear(calls)
		}
		calls[key] = true
	}
	if len(call.Output) > 0 && call.ID != "" {
		creates = creates || calls[key]
		delete(calls, key)
	}
	t.mu.Unlock()
	if len(call.Output) == 0 {
		return
	}
	// Linking reads the worktrees' origins and writes the store: off the
	// engine's event path.
	if found := workitems.Scan(call.output(), creates); len(found) > 0 {
		go b.linkFound(e, found)
	}
}

// toolCall is one tool event as the engine reports it: a call's input and
// its result may come apart, under the same id.
type toolCall struct {
	ID     string          `json:"id"`
	Name   string          `json:"name"`
	Status string          `json:"status"`
	Input  json.RawMessage `json:"input"`
	Output json.RawMessage `json:"output"`
}

func toolCallOf(ev agent.Event) (toolCall, bool) {
	var call toolCall
	if len(ev.Data) == 0 || json.Unmarshal(ev.Data, &call) != nil {
		return call, false
	}
	return call, true
}

// creates reports a call whose command opens a pull request.
func (c toolCall) creates() bool {
	return len(c.Input) > 0 && workitems.CreatesPullRequest(workitems.ToolCommand(c.Name, c.Input))
}

// output is the result's text (a JSON string decoded).
func (c toolCall) output() string {
	var text string
	if json.Unmarshal(c.Output, &text) == nil {
		return text
	}
	return string(c.Output)
}

func (b *Bench) linkFound(e *Execution, found []workitems.Found) {
	for _, f := range found {
		if f.Role == workitems.RoleMentions {
			// A pull request this workspace already delivers is not a mention.
			if item := b.workItems.store.Links(workitems.Filter{Workspace: e.CollectionID, Roles: []string{workitems.RoleDelivers, workitems.RoleOrigin}}); slices.ContainsFunc(item, func(l workitems.LinkView) bool { return l.Ref == f.Ref }) {
				continue
			}
		}
		_, _ = b.linkItem(workitems.Link{Ref: f.Ref, Workspace: e.CollectionID, Session: e.ID, Conversation: conversationOf(e), Role: f.Role, Source: workitems.SourceAgentOutput})
	}
}

// itemPipeFinished reads a finished `lazybox run` of an agent for a pull
// request it opened: the command line says it creates one, and its output
// ends with the URL.
func (b *Bench) itemPipeFinished(c *commands.Command) {
	state := c.Status()
	if b.workItems.store == nil || state.Spec.SessionID == "" || !workitems.CreatesPullRequest(state.Spec.Title+" "+strings.Join(state.Spec.Argv, " ")) {
		return
	}
	e, err := b.Get(state.Spec.SessionID)
	if err != nil || e.CollectionID == "" {
		return
	}
	ctx, cancel := context.WithTimeout(b.ctx, 5*time.Second)
	defer cancel()
	b.linkFound(e, workitems.Scan(string(c.Tail(ctx, 64<<10)), true))
}

// dispatchItemEvents sends each transition to the hooks that follow it, the
// delivering conversation's journal, and the subscribers (push).
func (b *Bench) dispatchItemEvents(events []workitems.Event) {
	if len(events) == 0 {
		return
	}
	b.workItems.mu.Lock()
	subscribers := slices.Clone(b.workItems.subscribers)
	b.workItems.mu.Unlock()
	for _, ev := range events {
		if b.power.Hooks != nil {
			hook := power.Event{Name: ev.Name, At: ev.At, Workspace: b.workspaceRef(ev.Workspace), Reason: ev.Reason, Line: ev.Title,
				PullRequest: &power.PullRequest{Repository: ev.Repository, Number: ev.Number, URL: ev.URL, Title: ev.Title, Head: ev.HeadSHA, Failed: ev.Failed},
				Key:         fmt.Sprintf("%s#%d@%s\x00%s\x00%s", ev.Repository, ev.Number, ev.HeadSHA, ev.Workspace, ev.Reason)}
			if ev.Name == workitems.EventChecksFailed {
				// The PR loop's own key, so one failure runs a hook once.
				hook.Key = fmt.Sprintf("%s#%d@%s", ev.Repository, ev.Number, ev.HeadSHA)
			}
			if ev.Session != "" {
				hook.Conversation = &power.Conversation{ID: ev.Conversation, Session: ev.Session}
			}
			go b.power.Hooks.Dispatch(hook)
		}
		b.journalItemEvent(ev)
		for _, fn := range subscribers {
			fn(ev)
		}
	}
}

// ItemEventRecord is the data of a "pull_request" record in a conversation.
type ItemEventRecord struct {
	Event      string   `json:"event"`
	Key        string   `json:"key"`
	Repository string   `json:"repository"`
	Number     int64    `json:"number"`
	Title      string   `json:"title,omitempty"`
	URL        string   `json:"url,omitempty"`
	Head       string   `json:"head,omitempty"`
	Reason     string   `json:"reason,omitempty"`
	Failed     []string `json:"failed,omitempty"`
	Workspace  string   `json:"workspace"`
}

// journalItemEvent puts the transition in the delivering conversation, as
// one quiet line (WatchEvents carries it to every window).
func (b *Bench) journalItemEvent(ev workitems.Event) {
	if ev.Session == "" {
		return
	}
	e, err := b.Get(ev.Session)
	if err != nil {
		return
	}
	head, err := b.latest(e)
	if err != nil || head.Archived || head.Engine.Snapshot().Closed {
		return
	}
	data, _ := json.Marshal(ItemEventRecord{Event: ev.Name, Key: ev.Ref.Key, Repository: ev.Repository, Number: ev.Number, Title: ev.Title, URL: ev.URL, Head: ev.HeadSHA, Reason: ev.Reason, Failed: ev.Failed, Workspace: ev.Workspace})
	_ = b.Record(head, agent.Event{Type: "pull_request", Text: fmt.Sprintf("%s#%d %s", ev.Repository, ev.Number, ev.Reason), Status: ev.Name, Data: data})
}
