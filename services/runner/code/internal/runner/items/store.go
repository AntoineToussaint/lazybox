package items

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"sync"
	"time"
)

// Bounds that keep the file small whatever agents print.
const (
	// mentionsPerWorkspace: the oldest mentions go first.
	mentionsPerWorkspace = 100
	// eventsKept: the newest transitions, for the Inbox and `lazybox items`.
	eventsKept = 200
	// branchesKept: the oldest branch sightings go first.
	branchesKept = 2000
	// linksPerWorkspace bounds one workspace's links, so what is linked to
	// one workspace never pushes another's out; its oldest go first.
	linksPerWorkspace = 500
	// What agents may say a workspace delivers (`lazybox link`, a pull
	// request their tools opened): every delivered pull request is read
	// from GitHub every minute or so, so an agent cannot make Lazybox track
	// without bound. The person's and the branch detector's links are not
	// counted.
	agentDeliversPerWorkspace = 50
	agentDeliversPerSession   = 20
	// backfilledKept bounds the conversation attempts whose journal read is
	// remembered.
	backfilledKept = 2000
)

// state is the file's content.
type state struct {
	Version  int                 `json:"version"`
	Links    []Link              `json:"links"`
	Items    map[string]Item     `json:"items"`
	Tracking map[string]Tracking `json:"tracking,omitempty"`
	Branches []Branch            `json:"branches,omitempty"`
	Events   []Event             `json:"events,omitempty"`
	// Backfilled is, per conversation attempt, the journal sequence read up
	// to for pull requests opened while no detector ran (Runner start).
	Backfilled map[string]uint64 `json:"backfilled,omitempty"`
}

// Tracking is how one item is kept fresh.
type Tracking struct {
	// ETag is the provider's validator for the last read (opaque).
	ETag      string    `json:"etag,omitempty"`
	CheckedAt time.Time `json:"checkedAt,omitzero"`
	// Failures counts reads in a row that failed; the next read backs off.
	Failures  int    `json:"failures,omitempty"`
	LastError string `json:"lastError,omitempty"`
}

// Store keeps links, snapshots, branch sightings and recent events in one
// private file, written atomically on every change.
type Store struct {
	mu   sync.Mutex
	path string
	s    state
	// Now is the clock; tests replace it.
	Now func() time.Time
}

// Open reads the store at path, or starts an empty one. An unreadable file is
// kept aside (path.corrupt) rather than silently overwritten.
func Open(path string) (*Store, error) {
	if !filepath.IsAbs(path) {
		return nil, errors.New("item store path must be absolute")
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, err
	}
	st := &Store{path: path, Now: time.Now, s: state{Version: 1, Items: map[string]Item{}, Tracking: map[string]Tracking{}}}
	raw, err := os.ReadFile(path)
	switch {
	case errors.Is(err, os.ErrNotExist):
		return st, nil
	case err != nil:
		return nil, err
	}
	var loaded state
	if err = json.Unmarshal(raw, &loaded); err != nil {
		_ = os.Rename(path, path+".corrupt")
		return st, nil
	}
	if loaded.Items == nil {
		loaded.Items = map[string]Item{}
	}
	if loaded.Tracking == nil {
		loaded.Tracking = map[string]Tracking{}
	}
	st.s = loaded
	return st, nil
}

func (s *Store) write() error {
	raw, err := json.Marshal(s.s)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(s.path), ".items-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(raw)
	if err == nil {
		err = f.Sync()
	}
	if err = errors.Join(err, f.Close()); err != nil {
		return err
	}
	return os.Rename(f.Name(), s.path)
}

// update applies change under the lock and persists it when it says so.
func (s *Store) update(change func(*state) bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !change(&s.s) {
		return nil
	}
	return s.write()
}

func newEventID() string {
	var b [6]byte
	_, _ = rand.Read(b[:])
	return "ie-" + hex.EncodeToString(b[:])
}

// ErrTooManyDelivered refuses an agent's link past what agents may say a
// workspace, or one conversation, delivers.
var ErrTooManyDelivered = fmt.Errorf("agents have linked as many delivered pull requests as Lazybox tracks for this workspace (%d) or conversation (%d); unlink some first", agentDeliversPerWorkspace, agentDeliversPerSession)

// AgentMade reports a link an agent made: `lazybox link` with its credential,
// or a pull request its tools opened.
func AgentMade(source string) bool {
	return source == SourceCLI || source == SourceAgentOutput
}

// Link records a link. The same item linked again to a workspace keeps its
// first link, except that a mention becomes delivers when something says the
// workspace delivered it, and a missing conversation or member is filled in.
// added reports a new link or a role upgrade. An agent's new delivers link
// past agentDeliversPerWorkspace or agentDeliversPerSession is refused
// (ErrTooManyDelivered).
func (s *Store) Link(l Link) (added bool, err error) {
	if !l.Ref.Valid() || l.Workspace == "" {
		return false, errors.New("a link needs an item and a workspace")
	}
	switch l.Role {
	case RoleOrigin, RoleDelivers, RoleMentions:
	default:
		return false, fmt.Errorf("unknown link role %q", l.Role)
	}
	if l.At.IsZero() {
		l.At = s.Now()
	}
	var refused error
	err = s.update(func(st *state) bool {
		capped := func() bool {
			if l.Role != RoleDelivers || !AgentMade(l.Source) {
				return false
			}
			workspace, session := 0, 0
			for _, other := range st.Links {
				if other.Role != RoleDelivers || !AgentMade(other.Source) || other.Workspace != l.Workspace || other.Deleted {
					continue
				}
				workspace++
				if l.Session != "" && other.Session == l.Session {
					session++
				}
			}
			return workspace >= agentDeliversPerWorkspace || session >= agentDeliversPerSession
		}
		for i := range st.Links {
			old := &st.Links[i]
			if old.Ref != l.Ref || old.Workspace != l.Workspace {
				continue
			}
			changed := false
			if old.Role == RoleMentions && l.Role == RoleDelivers {
				if capped() {
					refused = ErrTooManyDelivered
					return false
				}
				old.Role, old.Source, old.At = l.Role, l.Source, l.At
				added, changed = true, true
			}
			if old.Role == l.Role || (old.Role != RoleMentions && l.Role == RoleMentions) {
				if old.Session == "" && l.Session != "" {
					old.Session, old.Conversation, changed = l.Session, l.Conversation, true
				}
				if old.Member == "" && l.Member != "" {
					old.Member, changed = l.Member, true
				}
				if old.WorkspaceName == "" && l.WorkspaceName != "" {
					old.WorkspaceName, changed = l.WorkspaceName, true
				}
			}
			return changed
		}
		if capped() {
			refused = ErrTooManyDelivered
			return false
		}
		st.Links = append(st.Links, l)
		added = true
		pruneMentions(st, l.Workspace)
		pruneWorkspace(st, l.Workspace)
		return true
	})
	if refused != nil {
		return false, refused
	}
	return added, err
}

// pruneWorkspace drops a workspace's oldest links past linksPerWorkspace.
func pruneWorkspace(st *state, workspace string) {
	count := 0
	for _, l := range st.Links {
		if l.Workspace == workspace {
			count++
		}
	}
	for i := 0; count > linksPerWorkspace && i < len(st.Links); {
		if st.Links[i].Workspace == workspace {
			st.Links = slices.Delete(st.Links, i, i+1)
			count--
			continue
		}
		i++
	}
}

func pruneMentions(st *state, workspace string) {
	count := 0
	for _, l := range st.Links {
		if l.Workspace == workspace && l.Role == RoleMentions {
			count++
		}
	}
	for i := 0; count > mentionsPerWorkspace && i < len(st.Links); {
		if st.Links[i].Workspace == workspace && st.Links[i].Role == RoleMentions {
			st.Links = slices.Delete(st.Links, i, i+1)
			count--
			continue
		}
		i++
	}
}

// Unlink removes an item's links from a workspace.
func (s *Store) Unlink(workspace string, ref Ref) error {
	return s.update(func(st *state) bool {
		before := len(st.Links)
		st.Links = slices.DeleteFunc(st.Links, func(l Link) bool { return l.Workspace == workspace && l.Ref == ref })
		return len(st.Links) != before
	})
}

// WorkspaceGone marks a deleted workspace's links: delivered pull requests
// stay readable as history, mentions go. name is kept when given.
func (s *Store) WorkspaceGone(workspace, name string) error {
	return s.update(func(st *state) bool {
		changed := false
		kept := st.Links[:0]
		for _, l := range st.Links {
			if l.Workspace == workspace {
				if l.Role == RoleMentions {
					changed = true
					continue
				}
				if !l.Deleted {
					l.Deleted, changed = true, true
					if name != "" {
						l.WorkspaceName = name
					}
				}
			}
			kept = append(kept, l)
		}
		st.Links = kept
		before := len(st.Branches)
		st.Branches = slices.DeleteFunc(st.Branches, func(b Branch) bool { return b.Workspace == workspace })
		return changed || len(st.Branches) != before
	})
}

// LinkView is a link with its item's snapshot (an unread one has only its ref).
type LinkView struct {
	Link
	Item Item
}

// Filter selects links.
type Filter struct {
	// Workspace: one workspace's links; empty is every workspace's.
	Workspace string
	// Roles: only these roles; empty is every role.
	Roles []string
	// Live leaves out deleted workspaces' links.
	Live bool
}

// Links lists links with their snapshots, newest first.
func (s *Store) Links(f Filter) []LinkView {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []LinkView
	for _, l := range s.s.Links {
		if (f.Workspace != "" && l.Workspace != f.Workspace) || (len(f.Roles) > 0 && !slices.Contains(f.Roles, l.Role)) || (f.Live && l.Deleted) {
			continue
		}
		item, ok := s.s.Items[l.Ref.ID()]
		if !ok {
			item = Item{Ref: l.Ref}
		}
		out = append(out, LinkView{Link: l, Item: item})
	}
	sort.SliceStable(out, func(i, j int) bool { return out[i].At.After(out[j].At) })
	return out
}

// Item is an item's snapshot.
func (s *Store) Item(ref Ref) (Item, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	item, ok := s.s.Items[ref.ID()]
	return item, ok
}

// Tracked lists every item a workspace delivers or started from, with how
// it is tracked: what a provider keeps fresh. Mentions are never tracked.
func (s *Store) Tracked() []TrackedItem {
	s.mu.Lock()
	defer s.mu.Unlock()
	seen := map[string]bool{}
	var out []TrackedItem
	for _, l := range s.s.Links {
		if l.Role == RoleMentions || seen[l.Ref.ID()] {
			continue
		}
		seen[l.Ref.ID()] = true
		item, known := s.s.Items[l.Ref.ID()]
		if !known {
			item = Item{Ref: l.Ref}
		}
		out = append(out, TrackedItem{Item: item, Known: known, Tracking: s.s.Tracking[l.Ref.ID()]})
	}
	return out
}

// TrackedItem is an item a provider keeps fresh.
type TrackedItem struct {
	Item     Item
	Known    bool
	Tracking Tracking
}

// Observe records a fresh snapshot and returns its transitions, each placed
// in every workspace that delivers it (the delivering conversation first).
// etag is the provider's validator ("" keeps the old one). readAt is when the
// read began (zero: now, and never stale).
func (s *Store) Observe(item Item, etag string, readAt time.Time) ([]Event, error) {
	return s.ObserveAll([]Observation{{Item: item, ETag: etag, ReadAt: readAt}})
}

// Observation is one fresh read.
type Observation struct {
	Item Item
	ETag string
	// ReadAt is when the read began, on the store's clock (Now). A read that
	// began before the snapshot kept was observed is stale — two reads of
	// one item can finish out of order — and is dropped, so an older answer
	// never replaces a newer one or replays its transitions. Zero: not
	// checked, observed now.
	ReadAt time.Time
}

// ObserveAll records several reads with one write.
func (s *Store) ObserveAll(reads []Observation) ([]Event, error) {
	var out []Event
	now := s.Now()
	err := s.update(func(st *state) bool {
		for _, read := range reads {
			item := read.Item
			if !item.Ref.Valid() {
				continue
			}
			item.Title = trimTitle(item.Title)
			id := item.Ref.ID()
			var before *Item
			if old, ok := st.Items[id]; ok && old.Kind != "" {
				if !read.ReadAt.IsZero() && read.ReadAt.Before(old.ObservedAt) {
					continue
				}
				before = &old
			}
			if item.ObservedAt.IsZero() {
				item.ObservedAt = now
				if !read.ReadAt.IsZero() {
					item.ObservedAt = read.ReadAt
				}
			}
			st.Items[id] = item
			t := st.Tracking[id]
			t.CheckedAt, t.Failures, t.LastError = now, 0, ""
			if read.ETag != "" {
				t.ETag = read.ETag
			}
			st.Tracking[id] = t
			for _, ev := range Transitions(before, item) {
				ev.At = now
				for _, l := range st.Links {
					if l.Ref != item.Ref || l.Role == RoleMentions {
						continue
					}
					placed := ev
					placed.ID, placed.Workspace, placed.Session, placed.Conversation = newEventID(), l.Workspace, l.Session, l.Conversation
					out = append(out, placed)
					st.Events = append(st.Events, placed)
				}
			}
		}
		if len(st.Events) > eventsKept {
			st.Events = st.Events[len(st.Events)-eventsKept:]
		}
		return true
	})
	return out, err
}

// Unchanged records a read the provider answered with "not modified".
func (s *Store) Unchanged(ref Ref) error {
	now := s.Now()
	return s.update(func(st *state) bool {
		t := st.Tracking[ref.ID()]
		t.CheckedAt, t.Failures, t.LastError = now, 0, ""
		st.Tracking[ref.ID()] = t
		if item, ok := st.Items[ref.ID()]; ok {
			item.ObservedAt = now
			st.Items[ref.ID()] = item
		}
		return true
	})
}

// Failed records a read that failed, so the next one backs off.
func (s *Store) Failed(ref Ref, cause error) error {
	now := s.Now()
	return s.update(func(st *state) bool {
		t := st.Tracking[ref.ID()]
		t.CheckedAt = now
		t.Failures++
		if cause != nil {
			t.LastError = cause.Error()
		}
		st.Tracking[ref.ID()] = t
		return true
	})
}

// Events lists the latest transitions, newest first.
func (s *Store) Events(workspace string, limit int) []Event {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []Event
	for i := len(s.s.Events) - 1; i >= 0 && (limit <= 0 || len(out) < limit); i-- {
		if workspace == "" || s.s.Events[i].Workspace == workspace {
			out = append(out, s.s.Events[i])
		}
	}
	return out
}

// SeeBranch records that a workspace repository is on a branch. new reports
// a branch not seen before, or one newly pushed: worth asking about at once.
// Nothing is written when only the time it was last seen moved; that is
// persisted with the next change.
func (s *Store) SeeBranch(b Branch) (fresh bool, err error) {
	if b.Workspace == "" || b.Member == "" || b.Repository == "" || b.Name == "" {
		return false, nil
	}
	now := s.Now()
	err = s.update(func(st *state) bool {
		for i := range st.Branches {
			old := &st.Branches[i]
			if old.key() != b.key() {
				continue
			}
			old.LastSeen = now
			if b.Pushed && !old.Pushed {
				old.Pushed, fresh = true, true
			}
			return fresh
		}
		b.FirstSeen, b.LastSeen, b.CheckedAt, b.ETag = now, now, time.Time{}, ""
		st.Branches = append(st.Branches, b)
		if len(st.Branches) > branchesKept {
			st.Branches = st.Branches[len(st.Branches)-branchesKept:]
		}
		fresh = true
		return true
	})
	return fresh, err
}

// Branches lists the branch sightings.
func (s *Store) Branches() []Branch {
	s.mu.Lock()
	defer s.mu.Unlock()
	return slices.Clone(s.s.Branches)
}

// CheckedBranch records a branch lookup and its validator, or why it failed
// (cause): the next lookup backs off, and ListLinks says why.
func (s *Store) CheckedBranch(b Branch, etag string, cause error) error {
	now := s.Now()
	return s.update(func(st *state) bool {
		for i := range st.Branches {
			if st.Branches[i].key() != b.key() {
				continue
			}
			branch := &st.Branches[i]
			branch.CheckedAt = now
			if cause != nil {
				branch.Failures++
				branch.LastError = cause.Error()
				return true
			}
			branch.Failures, branch.LastError = 0, ""
			if etag != "" {
				branch.ETag = etag
			}
			return true
		}
		return false
	})
}

// Problem is why tracking could not read something of a workspace (of every
// workspace when workspace is empty): the last failed lookup of a branch it
// was on this week, else the last failed read of a pull request it
// delivers. Empty when the last reads all answered.
func (s *Store) Problem(workspace string, now time.Time) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	for i := len(s.s.Branches) - 1; i >= 0; i-- {
		b := s.s.Branches[i]
		if b.LastError != "" && (workspace == "" || b.Workspace == workspace) && now.Sub(b.LastSeen) <= BranchFor {
			return b.LastError
		}
	}
	for _, l := range s.s.Links {
		if l.Role == RoleMentions || l.Deleted || (workspace != "" && l.Workspace != workspace) {
			continue
		}
		if t := s.s.Tracking[l.Ref.ID()]; t.LastError != "" {
			return t.LastError
		}
	}
	return ""
}

// Backfilled is the journal sequence a conversation attempt was read up to
// for pull requests its agents opened (0 when never).
func (s *Store) Backfilled(session string) uint64 {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.s.Backfilled[session]
}

// SetBackfilled records that a conversation attempt's journal was read up to
// sequence. It never moves back.
func (s *Store) SetBackfilled(session string, sequence uint64) error {
	return s.update(func(st *state) bool {
		if sequence <= st.Backfilled[session] {
			return false
		}
		if st.Backfilled == nil {
			st.Backfilled = map[string]uint64{}
		}
		if _, ok := st.Backfilled[session]; !ok && len(st.Backfilled) >= backfilledKept {
			// Forget one: at worst its journal is read again, and linking
			// is idempotent.
			for id := range st.Backfilled {
				delete(st.Backfilled, id)
				break
			}
		}
		st.Backfilled[session] = sequence
		return true
	})
}

// BranchPullRequest is the pull request a workspace delivers from a branch
// of a repository: an open one first, else the most recently updated.
func (s *Store) BranchPullRequest(workspace, repository, branch string) (Item, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	var best Item
	found := false
	for _, l := range s.s.Links {
		if l.Workspace != workspace || l.Role != RoleDelivers {
			continue
		}
		item, ok := s.s.Items[l.Ref.ID()]
		if !ok || item.PR == nil || item.PR.Head != branch || !strings.EqualFold(item.Repository, repository) {
			continue
		}
		if !found || (item.RawState == "open" && best.RawState != "open") || (item.RawState == best.RawState && item.UpdatedAt.After(best.UpdatedAt)) {
			best, found = item, true
		}
	}
	return best, found
}

// Checked reports whether a branch lookup has answered for this branch.
func (s *Store) Checked(workspace, member, repository, branch string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, b := range s.s.Branches {
		if b.Workspace == workspace && b.Member == member && strings.EqualFold(b.Repository, repository) && b.Name == branch {
			return !b.CheckedAt.IsZero()
		}
	}
	return false
}
