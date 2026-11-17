package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/repositorycatalog"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Agents ask for a repository instead of reaching outside their workspace
// (WORKSPACES.md, "When an agent needs another repository"). The agent names
// what it needs and why (`lazybox workspace request-repo`); the Runner matches
// that against the repository catalog and records a request on the asking
// conversation, where the person adds the repository with one click, picks
// another, or declines. A request never adds anything: adding is the owner's
// PrepareWorkspace. The agent learns the outcome from its wait, or, when it is
// no longer waiting, from a short note sent into its conversation.

// Repository request states.
const (
	RequestPending  = "pending"
	RequestAdded    = "added"
	RequestDeclined = "declined"
	RequestExpired  = "expired"
)

const (
	// repositoryRequestTTL is how long a request waits for the person.
	repositoryRequestTTL = 24 * time.Hour
	// maxPendingRepositoryRequests bounds what one conversation has open at once…
	maxPendingRepositoryRequests = 3
	// …and maxRepositoryRequestsPerHour how many it may make in an hour.
	maxRepositoryRequestsPerHour = 10
	// MaxRepositoryRequestWait bounds one WaitRepositoryRequest call.
	MaxRepositoryRequestWait = 60 * time.Second
	// repositoryNoteGrace is how long an answer waits for a waiting agent to
	// collect it before it is sent into the conversation instead.
	repositoryNoteGrace = 3 * time.Second
	// resolvedRequestRetention keeps answered requests for `workspace repos`.
	resolvedRequestRetention = 7 * 24 * time.Hour
	maxRepositoryQuery       = 512
	maxRepositoryReason      = 1000
	maxRepositoryCandidates  = 5
)

// ErrRepositoryRequestLimit refuses a request over the rate limits.
var ErrRepositoryRequestLimit = errors.New("too many repository requests")

// ErrInvalidRepositoryRequest refuses a malformed request.
var ErrInvalidRepositoryRequest = errors.New("invalid repository request")

// RepositoryCandidate is a repository that matches what the agent asked for.
type RepositoryCandidate struct {
	// Identity is the catalog identity; empty for a local git folder that is
	// not in the catalog yet (LocalPath names it).
	Identity  string `json:"identity,omitempty"`
	Name      string `json:"name"`
	Source    string `json:"source"`
	LocalPath string `json:"localPath,omitempty"`
	Score     int    `json:"score"`
}

// WorkspaceRepository is one repository of a workspace.
type WorkspaceRepository struct {
	Name     string `json:"name"`
	Path     string `json:"path"`
	Branch   string `json:"branch"`
	Identity string `json:"identity,omitempty"`
	Ready    bool   `json:"ready"`
	// Home is the person's home checkout of the repository, "" when none:
	// theirs, read-only for agents (homes.go).
	Home         string `json:"home,omitempty"`
	RepositoryID string `json:"repositoryId,omitempty"`
}

// RepositoryRequest is one agent's request to add a repository to its workspace.
type RepositoryRequest struct {
	ID           string                `json:"id"`
	Session      string                `json:"session"`
	Conversation string                `json:"conversation"`
	Workspace    string                `json:"workspace"`
	Query        string                `json:"query"`
	Reason       string                `json:"reason,omitempty"`
	Candidates   []RepositoryCandidate `json:"candidates,omitempty"`
	State        string                `json:"state"`
	// Branch is the branch the repository gets in the workspace.
	Branch        string                `json:"branch,omitempty"`
	DeclineReason string                `json:"declineReason,omitempty"`
	Added         []WorkspaceRepository `json:"added,omitempty"`
	CreatedAt     time.Time             `json:"createdAt"`
	ExpiresAt     time.Time             `json:"expiresAt"`
	ResolvedAt    time.Time             `json:"resolvedAt,omitzero"`
	// Already: the repository was in the workspace; nothing was recorded.
	Already bool `json:"already,omitempty"`
	// Told: the agent has the outcome, from its wait or a note.
	Told bool `json:"told,omitempty"`
}

// Match is the repository the request proposes, if any.
func (r RepositoryRequest) Match() (RepositoryCandidate, bool) {
	if len(r.Candidates) == 0 {
		return RepositoryCandidate{}, false
	}
	return r.Candidates[0], true
}

// repositoryRequests keeps every request, persisted so a request (and an
// agent waiting on it) outlives a Runner restart.
type repositoryRequests struct {
	mu    sync.Mutex
	path  string
	items map[string]*RepositoryRequest
	// changed is closed and replaced whenever a request changes.
	changed chan struct{}
	// waiting counts the agents in WaitRepositoryRequest, per request.
	waiting map[string]int
	now     func() time.Time
	grace   time.Duration
}

func openRepositoryRequests(path string) (*repositoryRequests, error) {
	s := &repositoryRequests{path: path, items: map[string]*RepositoryRequest{}, changed: make(chan struct{}), waiting: map[string]int{}, now: time.Now, grace: repositoryNoteGrace}
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return s, nil
	}
	if err != nil {
		return nil, err
	}
	var saved []*RepositoryRequest
	if err = json.Unmarshal(data, &saved); err != nil {
		return nil, fmt.Errorf("repository requests %s: %w", path, err)
	}
	for _, r := range saved {
		s.items[r.ID] = r
	}
	return s, nil
}

// saveLocked writes every request, dropping answered ones past retention.
func (s *repositoryRequests) saveLocked() error {
	cutoff := s.now().Add(-resolvedRequestRetention)
	list := make([]*RepositoryRequest, 0, len(s.items))
	for id, r := range s.items {
		if r.State != RequestPending && !r.ResolvedAt.IsZero() && r.ResolvedAt.Before(cutoff) {
			delete(s.items, id)
			continue
		}
		list = append(list, r)
	}
	sort.Slice(list, func(i, j int) bool { return list[i].CreatedAt.Before(list[j].CreatedAt) })
	data, err := json.Marshal(list)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(s.path), 0o700); err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(s.path), ".repository-requests-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), s.path)
}

// broadcastLocked wakes every waiter.
func (s *repositoryRequests) broadcastLocked() {
	close(s.changed)
	s.changed = make(chan struct{})
}

// expireLocked ends a request the person did not answer in time.
func (s *repositoryRequests) expireLocked(r *RepositoryRequest) bool {
	if r.State != RequestPending || s.now().Before(r.ExpiresAt) {
		return false
	}
	r.State, r.ResolvedAt = RequestExpired, r.ExpiresAt
	return true
}

func (s *repositoryRequests) get(id string) (RepositoryRequest, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	r := s.items[id]
	if r == nil {
		return RepositoryRequest{}, fmt.Errorf("repository request %q: %w", id, os.ErrNotExist)
	}
	if s.expireLocked(r) {
		_ = s.saveLocked()
		s.broadcastLocked()
	}
	return *r, nil
}

// list returns the requests that pass keep, oldest first.
func (s *repositoryRequests) list(keep func(*RepositoryRequest) bool) []RepositoryRequest {
	s.mu.Lock()
	defer s.mu.Unlock()
	expired := false
	var out []RepositoryRequest
	for _, r := range s.items {
		expired = s.expireLocked(r) || expired
		if keep(r) {
			out = append(out, *r)
		}
	}
	if expired {
		_ = s.saveLocked()
		s.broadcastLocked()
	}
	sort.Slice(out, func(i, j int) bool { return out[i].CreatedAt.Before(out[j].CreatedAt) })
	return out
}

// RepositoryRequestSession returns the execution that made a request (for
// the transport's agent allowlist).
func (b *Bench) RepositoryRequestSession(id string) (string, error) {
	r, err := b.repositoryRequests.get(id)
	if err != nil {
		return "", err
	}
	return r.Session, nil
}

// GetRepositoryRequest returns one request, reconciled with its workspace.
func (b *Bench) GetRepositoryRequest(id string) (RepositoryRequest, error) {
	r, err := b.repositoryRequests.get(id)
	if err != nil || r.State != RequestPending {
		return r, err
	}
	b.reconcileRepositoryRequests(r.Workspace)
	return b.repositoryRequests.get(id)
}

// cleanText validates free text an agent sends: bounded, one paragraph of
// printable text.
func cleanText(text string, limit int, what string) (string, error) {
	text = strings.TrimSpace(text)
	if len(text) > limit || !utf8.ValidString(text) {
		return "", fmt.Errorf("%w: the %s must be at most %d bytes of text", ErrInvalidRepositoryRequest, what, limit)
	}
	if strings.IndexFunc(text, func(r rune) bool { return unicode.IsControl(r) && r != '\n' && r != '\t' }) >= 0 {
		return "", fmt.Errorf("%w: the %s contains control characters", ErrInvalidRepositoryRequest, what)
	}
	return text, nil
}

// workspaceMembers lists a workspace's repositories with their identities.
func (b *Bench) workspaceMembers(c collections.Collection) []WorkspaceRepository {
	out := make([]WorkspaceRepository, 0, len(c.Members))
	for _, m := range c.Members {
		out = append(out, b.workspaceMember(m))
	}
	return out
}

// memberFor returns the workspace member that is candidate, if any.
func (b *Bench) memberFor(c collections.Collection, candidate RepositoryCandidate) (WorkspaceRepository, bool) {
	for _, m := range c.Members {
		if candidate.Identity != "" && b.Collections.RepositoryIdentity(m.RepositoryID) == candidate.Identity {
			return b.workspaceMember(m), true
		}
		if candidate.LocalPath != "" {
			if repo, ok := b.Collections.Repository(m.RepositoryID); ok && filepath.Clean(repo.Path) == filepath.Clean(candidate.LocalPath) {
				return b.workspaceMember(m), true
			}
		}
	}
	return WorkspaceRepository{}, false
}

func (b *Bench) workspaceMember(m collections.Member) WorkspaceRepository {
	home, _ := b.Collections.Home(m.RepositoryID)
	return WorkspaceRepository{Name: m.Name, Path: m.Path, Branch: m.Branch, Identity: b.Collections.RepositoryIdentity(m.RepositoryID), Ready: m.Ready, Home: home, RepositoryID: m.RepositoryID}
}

// gitRoot is the git repository a local path is in, or "".
func gitRoot(path string) string {
	info, err := os.Stat(path)
	if err != nil {
		return ""
	}
	if !info.IsDir() {
		path = filepath.Dir(path)
	}
	for dir := path; ; dir = filepath.Dir(dir) {
		if _, err := os.Stat(filepath.Join(dir, ".git")); err == nil {
			return dir
		}
		if parent := filepath.Dir(dir); parent == dir {
			return ""
		}
	}
}

// matchRepository ranks the catalog (and, for a path, the git folder it
// names) against what the agent asked for.
func (b *Bench) matchRepository(ctx context.Context, query string) ([]RepositoryCandidate, error) {
	rows, _, err := b.RepositoryCatalogSnapshot(ctx, false)
	if err != nil {
		return nil, err
	}
	var out []RepositoryCandidate
	matched := repositorycatalog.Match(rows, query, maxRepositoryCandidates)
	if filepath.IsAbs(query) && (len(matched) == 0 || matched[0].Score < 95) {
		// A git folder the catalog does not know yet: the person can still add it.
		if root := gitRoot(filepath.Clean(query)); root != "" {
			out = append(out, RepositoryCandidate{Name: filepath.Base(root), Source: "folder", LocalPath: root, Score: 100})
		}
	}
	for _, c := range matched {
		out = append(out, RepositoryCandidate{Identity: c.Identity, Name: c.Name, Source: c.Source, LocalPath: c.LocalPath, Score: c.Score})
	}
	if len(out) > maxRepositoryCandidates {
		out = out[:maxRepositoryCandidates]
	}
	return out, nil
}

// RequestRepository records an agent's request to add a repository to its
// execution's workspace. It adds nothing. When the best match is already in
// the workspace it says so and records nothing; a request for the same
// repository that is still waiting is returned rather than repeated.
func (b *Bench) RequestRepository(ctx context.Context, session, query, reason string) (RepositoryRequest, error) {
	e, err := b.Get(session)
	if err != nil {
		return RepositoryRequest{}, err
	}
	if e.CollectionID == "" || b.Collections == nil {
		return RepositoryRequest{}, fmt.Errorf("%w: this conversation does not work in a Lazybox workspace", ErrInvalidRepositoryRequest)
	}
	if query, err = cleanText(query, maxRepositoryQuery, "repository"); err != nil {
		return RepositoryRequest{}, err
	}
	if query == "" || strings.ContainsRune(query, '\n') {
		return RepositoryRequest{}, fmt.Errorf("%w: name one repository: a name, owner/repo, GitHub URL or absolute path", ErrInvalidRepositoryRequest)
	}
	if reason, err = cleanText(reason, maxRepositoryReason, "reason"); err != nil {
		return RepositoryRequest{}, err
	}
	workspace, err := b.Collections.Get(e.CollectionID)
	if err != nil {
		return RepositoryRequest{}, err
	}
	if workspace.State == "deleted" || workspace.State == "deleting" || workspace.State == "archived" {
		return RepositoryRequest{}, fmt.Errorf("%w: this workspace is %s", ErrInvalidRepositoryRequest, workspace.State)
	}
	candidates, err := b.matchRepository(ctx, query)
	if err != nil {
		return RepositoryRequest{}, err
	}
	now := b.repositoryRequests.now()
	r := RepositoryRequest{Session: e.ID, Conversation: conversationOf(e), Workspace: workspace.ID, Query: query, Reason: reason, Candidates: candidates, State: RequestPending, Branch: workspace.Branch(), CreatedAt: now, ExpiresAt: now.Add(repositoryRequestTTL)}
	if top, ok := r.Match(); ok {
		if member, in := b.memberFor(workspace, top); in {
			r.State, r.Already, r.Added, r.ResolvedAt, r.Told = RequestAdded, true, []WorkspaceRepository{member}, now, true
			return r, nil
		}
	}
	// Other matches already in the workspace are not offered.
	kept := r.Candidates[:0]
	for i, c := range r.Candidates {
		if _, in := b.memberFor(workspace, c); i == 0 || !in {
			kept = append(kept, c)
		}
	}
	r.Candidates = kept

	s := b.repositoryRequests
	s.mu.Lock()
	defer s.mu.Unlock()
	pending, recent := 0, 0
	for _, other := range s.items {
		s.expireLocked(other)
		if other.Conversation != r.Conversation {
			continue
		}
		if other.State == RequestPending {
			if sameRepository(*other, r) {
				return *other, nil
			}
			pending++
		}
		if now.Sub(other.CreatedAt) < time.Hour {
			recent++
		}
	}
	if pending >= maxPendingRepositoryRequests {
		return RepositoryRequest{}, fmt.Errorf("%w: %d requests already wait for the person in this conversation; wait for an answer (lazybox workspace repos lists them)", ErrRepositoryRequestLimit, pending)
	}
	if recent >= maxRepositoryRequestsPerHour {
		return RepositoryRequest{}, fmt.Errorf("%w: at most %d per hour per conversation", ErrRepositoryRequestLimit, maxRepositoryRequestsPerHour)
	}
	r.ID = "repo-" + strings.ToLower(commands.ID())
	s.items[r.ID] = &r
	if err := s.saveLocked(); err != nil {
		delete(s.items, r.ID)
		return RepositoryRequest{}, err
	}
	s.broadcastLocked()
	return r, nil
}

// sameRepository: two requests for the same repository (or, without a
// match, the same words).
func sameRepository(a, b RepositoryRequest) bool {
	ma, oka := a.Match()
	mb, okb := b.Match()
	if oka && okb {
		return ma.Identity == mb.Identity && ma.LocalPath == mb.LocalPath
	}
	return !oka && !okb && strings.EqualFold(a.Query, b.Query)
}

// WaitRepositoryRequest waits, at most timeout, for the person's answer. An
// answer collected here is the agent's: no note is sent for it.
func (b *Bench) WaitRepositoryRequest(ctx context.Context, id string, timeout time.Duration) (RepositoryRequest, error) {
	timeout = min(max(timeout, 0), MaxRepositoryRequestWait)
	s := b.repositoryRequests
	s.mu.Lock()
	s.waiting[id]++
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		if s.waiting[id]--; s.waiting[id] <= 0 {
			delete(s.waiting, id)
		}
		s.mu.Unlock()
	}()
	deadline := time.NewTimer(timeout)
	defer deadline.Stop()
	// Additions made outside Lazybox's own path (another client, the CLI) are
	// noticed by looking again now and then.
	tick := time.NewTicker(2 * time.Second)
	defer tick.Stop()
	for {
		r, err := b.GetRepositoryRequest(id)
		if err != nil {
			return r, err
		}
		s.mu.Lock()
		changed := s.changed
		if current := s.items[id]; current != nil && current.State != RequestPending && current.State != RequestExpired && !current.Told {
			current.Told = true
			_ = s.saveLocked()
			r = *current
		}
		s.mu.Unlock()
		if r.State != RequestPending {
			return r, nil
		}
		select {
		case <-changed:
		case <-tick.C:
		case <-deadline.C:
			return r, nil
		case <-ctx.Done():
			return r, ctx.Err()
		case <-b.ctx.Done():
			return r, errors.New("the Runner is stopping; ask again in a moment")
		}
	}
}

// DeclineRepositoryRequest is the person's no, with their reason.
func (b *Bench) DeclineRepositoryRequest(id, reason string) (RepositoryRequest, error) {
	reason, err := cleanText(reason, maxRepositoryReason, "reason")
	if err != nil {
		return RepositoryRequest{}, err
	}
	return b.resolveRepositoryRequest(id, func(r *RepositoryRequest) error {
		if r.State != RequestPending {
			return fmt.Errorf("this request was already %s", r.State)
		}
		r.State, r.DeclineReason = RequestDeclined, reason
		return nil
	})
}

// resolveRepositoryRequest applies an answer, wakes a waiting agent, and
// otherwise sends the answer into the conversation once no wait collects it.
func (b *Bench) resolveRepositoryRequest(id string, answer func(*RepositoryRequest) error) (RepositoryRequest, error) {
	s := b.repositoryRequests
	s.mu.Lock()
	r := s.items[id]
	if r == nil {
		s.mu.Unlock()
		return RepositoryRequest{}, fmt.Errorf("repository request %q: %w", id, os.ErrNotExist)
	}
	s.expireLocked(r)
	before := *r
	if err := answer(r); err != nil {
		*r = before
		s.mu.Unlock()
		return before, err
	}
	r.ResolvedAt = s.now()
	if err := s.saveLocked(); err != nil {
		*r = before
		s.mu.Unlock()
		return before, err
	}
	s.broadcastLocked()
	out := *r
	s.mu.Unlock()
	b.noteLater(id)
	return out, nil
}

// noteLater sends a resolved request's answer into its conversation unless a
// waiting agent collects it first.
func (b *Bench) noteLater(id string) {
	s := b.repositoryRequests
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		timer := time.NewTimer(s.grace)
		defer timer.Stop()
		select {
		case <-timer.C:
		case <-b.ctx.Done():
			return
		}
		s.mu.Lock()
		r := s.items[id]
		if r == nil || r.Told || r.State == RequestPending || r.State == RequestExpired || s.waiting[id] > 0 {
			s.mu.Unlock()
			return
		}
		r.Told = true
		_ = s.saveLocked()
		note, origin := repositoryNote(*r)
		s.mu.Unlock()
		b.deliverNote(*r, note, origin)
	}()
}

// repositoryNote is what the agent is told when it no longer waits, and the
// event the conversation shows for it.
func repositoryNote(r RepositoryRequest) (string, *agent.Origin) {
	asked := r.Query
	if top, ok := r.Match(); ok {
		asked = top.Name
	}
	switch r.State {
	case RequestAdded:
		var lines, names, branches, paths []string
		for _, a := range r.Added {
			lines = append(lines, fmt.Sprintf("- %s at %s (branch %s)", a.Name, a.Path, a.Branch))
			names = append(names, a.Name)
			paths = append(paths, a.Path)
			if a.Branch != "" && !slices.Contains(branches, a.Branch) {
				branches = append(branches, a.Branch)
			}
		}
		if len(names) == 0 {
			names = []string{asked}
		}
		origin := &agent.Origin{Kind: agent.OriginRepositoryAdded, Summary: fmt.Sprintf("Added %s to this workspace", strings.Join(names, ", ")), Detail: strings.Join(branches, ", "), Copy: strings.Join(paths, "\n")}
		return fmt.Sprintf("[Lazybox] You asked to add %s to this workspace. The person added it:\n%s\nContinue with what you needed it for.", asked, strings.Join(lines, "\n")), origin
	case RequestDeclined:
		why := ""
		if r.DeclineReason != "" {
			why = " Their reason: " + r.DeclineReason
		}
		origin := &agent.Origin{Kind: agent.OriginRepositoryDeclined, Summary: fmt.Sprintf("Declined adding %s to this workspace", asked), Detail: r.DeclineReason}
		return fmt.Sprintf("[Lazybox] You asked to add %s to this workspace. The person declined.%s Do not read or change it outside the workspace; continue without it or ask them how to proceed.", asked, why), origin
	}
	return "", nil
}

// deliverNote sends note to the conversation's live agent, as a message
// (queued while it works). A conversation that has ended gets nothing: its
// next agent can run `lazybox workspace repos`.
func (b *Bench) deliverNote(r RepositoryRequest, note string, origin *agent.Origin) {
	if note == "" {
		return
	}
	target, err := b.Get(r.Session)
	if err != nil || target.Engine.Snapshot().Closed {
		target = nil
		for _, e := range b.List() {
			if conversationOf(e) == r.Conversation && !e.Engine.Snapshot().Closed {
				target = e
				break
			}
		}
	}
	if target == nil {
		return
	}
	ctx, cancel := context.WithTimeout(b.ctx, 10*time.Second)
	defer cancel()
	id := "repository-note-" + r.ID
	if receipt, err := target.Engine.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: note, Origin: origin}); err == nil && receipt.Error == "" {
		b.RecordPrompt(target, id, note, work.PromptSource{Kind: work.SourceRepository})
	}
}

// reconcileRepositoryRequests resolves a workspace's waiting requests whose
// repository is now in it, however it was added.
func (b *Bench) reconcileRepositoryRequests(workspaceID string) {
	if b.Collections == nil || workspaceID == "" {
		return
	}
	pending := b.repositoryRequests.list(func(r *RepositoryRequest) bool {
		return r.Workspace == workspaceID && r.State == RequestPending && len(r.Candidates) > 0
	})
	if len(pending) == 0 {
		return
	}
	workspace, err := b.Collections.Get(workspaceID)
	if err != nil {
		return
	}
	for _, r := range pending {
		top, _ := r.Match()
		if member, in := b.memberFor(workspace, top); in {
			_, _ = b.resolveRepositoryRequest(r.ID, func(current *RepositoryRequest) error {
				if current.State != RequestPending {
					return errors.New("answered meanwhile")
				}
				current.State, current.Added = RequestAdded, []WorkspaceRepository{member}
				return nil
			})
		}
	}
}

// checkRepositoryRequest validates the request an addition answers: it
// belongs to workspaceID and still waits (or was already answered by this
// addition, when a retry repeats it).
func (b *Bench) checkRepositoryRequest(id, workspaceID string) error {
	if id == "" {
		return nil
	}
	r, err := b.repositoryRequests.get(id)
	if err != nil {
		return err
	}
	if r.Workspace != workspaceID || workspaceID == "" {
		return fmt.Errorf("%w: that request is for another workspace", ErrInvalidRepositoryRequest)
	}
	if r.State != RequestPending && r.State != RequestAdded {
		return fmt.Errorf("that request was %s; ask the agent to request it again", r.State)
	}
	return nil
}

// repositoriesAdded settles requests after repositories were added to a
// workspace: the one the person answered (requestID) with what was added, and
// any other waiting request for a repository now in it.
func (b *Bench) repositoriesAdded(workspace collections.Collection, requestID string, added []string) {
	if requestID != "" {
		var members []WorkspaceRepository
		for _, m := range workspace.Members {
			for _, name := range added {
				if m.Name == name {
					members = append(members, b.workspaceMember(m))
				}
			}
		}
		if len(members) > 0 {
			_, _ = b.resolveRepositoryRequest(requestID, func(r *RepositoryRequest) error {
				if r.State != RequestPending {
					return errors.New("already answered")
				}
				r.State, r.Added = RequestAdded, members
				return nil
			})
		}
	}
	b.reconcileRepositoryRequests(workspace.ID)
}

// PendingRepositoryRequests are the requests waiting in e's conversation.
func (b *Bench) PendingRepositoryRequests(e *Execution) []RepositoryRequest {
	if b.repositoryRequests == nil {
		return nil
	}
	conversation := conversationOf(e)
	has := false
	b.repositoryRequests.mu.Lock()
	for _, r := range b.repositoryRequests.items {
		if r.Conversation == conversation && r.State == RequestPending {
			has = true
			break
		}
	}
	b.repositoryRequests.mu.Unlock()
	if !has {
		return nil
	}
	b.reconcileRepositoryRequests(e.CollectionID)
	return b.repositoryRequests.list(func(r *RepositoryRequest) bool {
		return r.Conversation == conversation && r.State == RequestPending
	})
}

// WorkspaceRepositories is a workspace's repositories, its branch and the
// requests waiting in it.
func (b *Bench) WorkspaceRepositories(workspaceID string) (collections.Collection, []WorkspaceRepository, []RepositoryRequest, error) {
	if b.Collections == nil {
		return collections.Collection{}, nil, nil, errors.New("workspace storage unavailable")
	}
	workspace, err := b.Collections.Get(workspaceID)
	if err != nil {
		return workspace, nil, nil, err
	}
	b.reconcileRepositoryRequests(workspaceID)
	requests := b.repositoryRequests.list(func(r *RepositoryRequest) bool {
		return r.Workspace == workspaceID && r.State == RequestPending
	})
	return workspace, b.workspaceMembers(workspace), requests, nil
}
