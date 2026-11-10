// Package collections owns durable, Runner-local groups of Git worktrees.
// Collection lifetime is independent of an agent, a window, and session state.
package collections

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	petname "github.com/dustinkirkland/golang-petname"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode"

	"github.com/AntoineToussaint/lazybox/v2/internal/gitidentity"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
)

type Repository struct {
	Hidden    bool   `json:"hidden,omitempty"`
	ID        string `json:"id"`
	Path      string `json:"path"`
	Name      string `json:"name,omitempty"`
	SourceURL string `json:"sourceUrl,omitempty"`
	GitHubID  int64  `json:"githubId,omitempty"`
}
type MemberSpec struct {
	Name         string `json:"name"`
	RepositoryID string `json:"repositoryId"`
	BaseRef      string `json:"baseRef,omitempty"`
	Branch       string `json:"branch,omitempty"`
}
type Spec struct {
	Name    string       `json:"name"`
	WorkID  string       `json:"workId,omitempty"`
	Members []MemberSpec `json:"members"`
}
type Member struct {
	MemberSpec
	Path       string `json:"path"`
	BaseCommit string `json:"baseCommit"`
	CommonDir  string `json:"commonDir"`
	Ready      bool   `json:"ready"`
}
type Collection struct {
	Slug                    string            `json:"slug,omitempty"`
	LegacyRoot              bool              `json:"legacyRoot,omitempty"`
	Version                 int               `json:"version"`
	ID                      string            `json:"id"`
	Name                    string            `json:"name"`
	WorkID                  string            `json:"workId"`
	Root                    string            `json:"root"`
	State                   string            `json:"state"`
	Error                   string            `json:"error,omitempty"`
	Members                 []Member          `json:"members"`
	RequestKey              string            `json:"requestKey"`
	Fingerprint             string            `json:"fingerprint"`
	OwnerExecutionID        string            `json:"ownerExecutionId,omitempty"`
	OwnerExecutionIDs       []string          `json:"ownerExecutionIds,omitempty"`
	AddRequests             map[string]string `json:"addRequests,omitempty"`
	OwnershipUncertain      bool              `json:"-"`
	LastOwnershipResolution string            `json:"lastOwnershipResolution,omitempty"`
}

var component = regexp.MustCompile(`^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$`)

// Manager serializes provisioning and attachment. The filesystem lock excludes
// other Runners even when they have different session-state directories.
type Manager struct {
	mu     sync.Mutex
	root   string
	lock   *os.File
	repos  map[string]Repository
	common map[string]string
	items  map[string]Collection
	ctx    context.Context
	cancel context.CancelFunc
	closed bool
	// adopted holds owners whose live agents this Runner re-attached: their
	// ownership is certain even though a previous Runner recorded it.
	adopted map[string]bool
}

func New(root string, repositories []Repository) (*Manager, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("collection worktreeRoot must be absolute")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	root, err := filepath.EvalSymlinks(root)
	if err != nil {
		return nil, err
	}
	meta := filepath.Join(root, ".lazybox-collections")
	if err = os.MkdirAll(filepath.Join(meta, "records"), 0700); err != nil {
		return nil, err
	}
	lock, err := os.OpenFile(filepath.Join(meta, "lock"), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, err
	}
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		lock.Close()
		return nil, errors.New("worktree root already owned by another Runner")
	}
	ctx, cancel := context.WithCancel(context.Background())
	m := &Manager{root: root, lock: lock, repos: map[string]Repository{}, common: map[string]string{}, items: map[string]Collection{}, ctx: ctx, cancel: cancel}
	fail := func(err error) (*Manager, error) { m.Close(); return nil, err }
	libraryData, loadErr := os.ReadFile(filepath.Join(meta, "repositories.json"))
	if loadErr != nil && !errors.Is(loadErr, os.ErrNotExist) {
		return fail(loadErr)
	}
	if loadErr == nil {
		var library []Repository
		if len(libraryData) > 1<<20 || json.Unmarshal(libraryData, &library) != nil || len(library) > 512 {
			return fail(errors.New("invalid repository library; preserve it for inspection"))
		}
		configured := map[string]Repository{}
		for _, r := range repositories {
			configured[r.ID] = r
		}
		for _, r := range library {
			if prior, ok := configured[r.ID]; ok {
				canonical, resolveErr := filepath.EvalSymlinks(prior.Path)
				if resolveErr != nil || canonical != r.Path {
					return fail(errors.New("repository library conflicts with Runner configuration"))
				}
				for i := range repositories {
					if repositories[i].ID == r.ID {
						repositories[i].Hidden = r.Hidden
					}
				}
				continue
			}
			repositories = append(repositories, r)
		}
	}
	for _, r := range repositories {
		if !component.MatchString(r.ID) || m.repos[r.ID].ID != "" || !filepath.IsAbs(r.Path) {
			return fail(errors.New("invalid or duplicate repository id or non-absolute path"))
		}
		r.Path, err = filepath.EvalSymlinks(r.Path)
		if err != nil {
			return fail(err)
		}
		common, err := m.git(ctx, r.Path, "rev-parse", "--path-format=absolute", "--git-common-dir")
		if err != nil {
			return fail(fmt.Errorf("repository %s: %w", r.ID, err))
		}
		common, err = filepath.EvalSymlinks(common)
		if err != nil {
			return fail(err)
		}
		m.repos[r.ID], m.common[r.ID] = m.resolveRepositorySource(ctx, r), common
	}
	entries, err := os.ReadDir(filepath.Join(meta, "records"))
	if err != nil {
		return fail(err)
	}
	if len(entries) > 16384 {
		return fail(errors.New("collection record capacity exceeded"))
	}
	keys, work := map[string]bool{}, map[string]bool{}
	for _, entry := range entries {
		// Atomic-write temporaries left by a crash never become records.
		if strings.HasPrefix(entry.Name(), ".record-") {
			continue
		}
		if !strings.HasSuffix(entry.Name(), ".json") {
			return fail(errors.New("unexpected collection record"))
		}
		f, err := os.Open(filepath.Join(meta, "records", entry.Name()))
		if err != nil {
			return fail(err)
		}
		data, err := io.ReadAll(io.LimitReader(f, (1<<20)+1))
		f.Close()
		if err != nil {
			return fail(err)
		}
		var c Collection
		if len(data) > 1<<20 || json.Unmarshal(data, &c) != nil || (c.Version != 1 && c.Version != 2 && c.Version != 3 && c.Version != 4) || !component.MatchString(c.ID) || c.ID+".json" != entry.Name() || !validRoot(root, c) || len(c.Members) > 64 || c.RequestKey == "" || keys[c.RequestKey] || c.WorkID == "" || work[c.WorkID] {
			return fail(errors.New("invalid collection record; preserve it for inspection"))
		}
		if c.State != "creating" && c.State != "ready" && c.State != "failed" && c.State != "archived" && c.State != "deleting" && c.State != "deleted" {
			return fail(errors.New("invalid collection state"))
		}
		names := map[string]bool{}
		for _, member := range c.Members {
			name := strings.ToLower(member.Name)
			if !component.MatchString(member.Name) || names[name] || member.Path != filepath.Join(c.Root, member.Name) || member.CommonDir == "" || member.BaseCommit == "" || member.Branch == "" {
				return fail(errors.New("invalid collection member record"))
			}
			names[name] = true
		}
		// The old Runner may have died without stopping its children. A missing
		// in-memory execution is not evidence that those writers have exited.
		if len(c.OwnerExecutionIDs) == 0 && c.OwnerExecutionID != "" {
			c.OwnerExecutionIDs = []string{c.OwnerExecutionID}
		}
		owners := map[string]bool{}
		for _, owner := range c.OwnerExecutionIDs {
			if !component.MatchString(owner) || owners[owner] {
				return fail(errors.New("invalid workspace session ownership"))
			}
			owners[owner] = true
		}
		if len(c.OwnerExecutionIDs) > 64 {
			return fail(errors.New("workspace session capacity exceeded"))
		}
		c.OwnershipUncertain = len(c.OwnerExecutionIDs) > 0
		if c.Version < 4 {
			c.Slug = filepath.Base(c.Root)
		}
		if c.Version == 1 {
			c.LegacyRoot = true
		}
		c.syncOwners()
		keys[c.RequestKey], work[c.WorkID], m.items[c.ID] = true, true, c
	}
	return m, nil
}

func (m *Manager) Repositories() []Repository {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.repositories()
}

// Keep historical IDs available for existing workspaces, but expose one library
// entry per logical repository. Sorted IDs make selection stable across restart.
func (m *Manager) repositories() []Repository {
	out := make([]Repository, 0, len(m.repos))
	for _, r := range m.repos {
		if r.Hidden {
			continue
		}
		out = append(out, r)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
	unique := out[:0]
	seen := map[string]bool{}
	for _, r := range out {
		identity := gitidentity.Repository(m.common[r.ID], r.SourceURL)
		if !seen[identity] {
			unique = append(unique, r)
			seen[identity] = true
		}
	}
	return unique
}
func clone(c Collection) Collection {
	c.Members = append([]Member(nil), c.Members...)
	c.OwnerExecutionIDs = append([]string(nil), c.OwnerExecutionIDs...)
	requests := map[string]string{}
	for key, value := range c.AddRequests {
		requests[key] = value
	}
	c.AddRequests = requests
	return c
}
func (c *Collection) syncOwners() {
	if c.Slug == "" {
		c.Slug = filepath.Base(c.Root)
	}
	c.Version = 4
	c.OwnerExecutionID = ""
	if len(c.OwnerExecutionIDs) > 0 {
		c.OwnerExecutionID = c.OwnerExecutionIDs[0]
	}
}
func (m *Manager) List() []Collection {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]Collection, 0, len(m.items))
	for _, c := range m.items {
		if c.State != "deleted" {
			out = append(out, clone(c))
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
	return out
}
func (m *Manager) Get(id string) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return Collection{}, os.ErrNotExist
	}
	return clone(c), nil
}

// Create pins all base commits before provisioning. A failed or interrupted
// request retains its record and any completed worktrees; the same key retries
// that plan. It never force-checks out, resets, removes, or prunes work.
func (m *Manager) Create(ctx context.Context, key string, spec Spec) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	spec.Members = append([]MemberSpec(nil), spec.Members...)
	if m.closed {
		return Collection{}, errors.New("collection manager closed")
	}
	if strings.TrimSpace(key) == "" || len(key) > 128 || strings.TrimSpace(spec.Name) == "" || len(spec.Name) > 128 || strings.IndexFunc(spec.Name, unicode.IsControl) >= 0 || len(spec.WorkID) > 512 || len(spec.Members) > 64 {
		return Collection{}, errors.New("request key, collection name and at most 64 worktrees required")
	}
	for i := range spec.Members {
		if spec.Members[i].BaseRef == "" {
			spec.Members[i].BaseRef = "HEAD"
		}
	}
	data, _ := json.Marshal(spec)
	hash := sha256.Sum256(data)
	fingerprint := hex.EncodeToString(hash[:])
	for _, c := range m.items {
		if c.RequestKey == key {
			if c.Fingerprint != fingerprint {
				return Collection{}, errors.New("request key already used for a different collection specification")
			}
			if c.State == "deleted" || c.State == "deleting" {
				return Collection{}, errors.New("workspace was deleted or cleanup is in progress")
			}
			if c.State == "ready" || c.State == "archived" {
				return clone(c), nil
			}
			return m.provision(ctx, c)
		}
		if spec.WorkID != "" && c.WorkID == spec.WorkID {
			return Collection{}, errors.New("work record already has a collection")
		}
	}
	activeCount := 0
	for _, item := range m.items {
		if item.State != "deleted" {
			activeCount++
		}
	}
	if activeCount >= 128 || len(m.items) >= 16384 {
		return Collection{}, errors.New("collection capacity exhausted")
	}
	c := Collection{Version: 4, ID: commands.ID(), Name: spec.Name, WorkID: spec.WorkID, State: "creating", RequestKey: key, Fingerprint: fingerprint}
	for attempt := 0; attempt < 32; attempt++ {
		slug := petname.Generate(3, "-")
		candidate := filepath.Join(m.root, slug)
		used := false
		for _, existing := range m.items {
			if strings.EqualFold(existing.Root, candidate) {
				used = true
				break
			}
		}
		if _, err := os.Lstat(candidate); !errors.Is(err, os.ErrNotExist) {
			used = true
		}
		if !used && component.MatchString(slug) {
			c.Slug = slug
			c.Root = candidate
			break
		}
	}
	if c.Root == "" {
		return Collection{}, errors.New("could not allocate a unique workspace folder")
	}

	if c.WorkID == "" {
		c.WorkID = c.ID
	}
	c, err := m.planMembers(ctx, c, spec.Members)
	if err != nil {
		return Collection{}, err
	}
	if err := m.save(c); err != nil {
		return Collection{}, err
	}
	return m.provision(ctx, c)
}

// AddRepositories extends the existing root; retries resume the same pinned plan.
// Existing worktrees and sessions are never recreated or moved.
func (m *Manager) AddRepositories(ctx context.Context, id, key string, specs []MemberSpec) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return Collection{}, os.ErrNotExist
	}
	c = clone(c)
	if m.closed || c.OwnershipUncertain || c.State == "archived" {
		return Collection{}, errors.New("workspace is not available for repository additions")
	}
	if strings.TrimSpace(key) == "" || len(key) > 128 || len(specs) == 0 || len(specs) > 64 {
		return Collection{}, errors.New("request key and repositories required")
	}
	data, _ := json.Marshal(specs)
	hash := sha256.Sum256(data)
	fingerprint := hex.EncodeToString(hash[:])
	if previous, ok := c.AddRequests[key]; ok {
		if previous != fingerprint {
			return Collection{}, errors.New("request key already used for different repositories")
		}
		if c.State == "ready" {
			return c, nil
		}
		return m.provision(ctx, c)
	}
	if c.State != "ready" || len(c.Members)+len(specs) > 64 || len(c.AddRequests) >= 128 {
		return Collection{}, errors.New("workspace must be ready and within repository capacity")
	}
	var err error
	c, err = m.planMembers(ctx, c, specs)
	if err != nil {
		return Collection{}, err
	}
	c.AddRequests[key] = fingerprint
	c.State = "creating"
	c.Version = 4
	if err = m.save(c); err != nil {
		return Collection{}, err
	}
	return m.provision(ctx, c)
}

func (m *Manager) planMembers(ctx context.Context, c Collection, specs []MemberSpec) (Collection, error) {
	names, branches := map[string]bool{}, map[string]bool{}
	for _, member := range c.Members {
		names[strings.ToLower(member.Name)] = true
		branches[member.CommonDir+"\x00"+member.Branch] = true
	}
	for _, s := range specs {
		if s.BaseRef == "" {
			s.BaseRef = "HEAD"
		}
		if len(s.BaseRef) > 1024 || len(s.Branch) > 256 {
			return Collection{}, errors.New("base ref or branch exceeds collection limits")
		}
		r, ok := m.repos[s.RepositoryID]
		if !ok {
			return Collection{}, fmt.Errorf("unknown repository %q", s.RepositoryID)
		}
		if s.Name == "" {
			s.Name = r.Name
			if s.Name == "" {
				s.Name = r.ID
			}
			if names[strings.ToLower(s.Name)] {
				base := s.Name
				for suffix := 2; names[strings.ToLower(s.Name)]; suffix++ {
					s.Name = fmt.Sprintf("%s-%d", base, suffix)
				}
			}
		}
		if !component.MatchString(s.Name) || names[strings.ToLower(s.Name)] {
			return Collection{}, errors.New("worktree names must be unique safe path components (case insensitive)")
		}
		names[strings.ToLower(s.Name)] = true
		if s.Branch == "" {
			s.Branch = "feature/" + filepath.Base(c.Root)
			if branches[m.common[r.ID]+"\x00"+s.Branch] {
				s.Branch += "-" + s.Name
			}
		}
		if _, err := m.git(ctx, r.Path, "check-ref-format", "refs/heads/"+s.Branch); err != nil {
			return Collection{}, fmt.Errorf("invalid branch: %w", err)
		}
		branchKey := m.common[r.ID] + "\x00" + s.Branch
		if branches[branchKey] {
			return Collection{}, errors.New("two worktrees cannot own the same branch")
		}
		branches[branchKey] = true
		// Explicit branches are new branches too. Existing branches are never
		// taken over; that requires a separate, deliberate adoption operation.
		exists, err := m.branchExists(ctx, r.Path, s.Branch)
		if err != nil {
			return Collection{}, err
		}
		if exists {
			return Collection{}, errors.New("branch already exists; choose a new collection branch")
		}
		commit, err := m.git(ctx, r.Path, "rev-parse", "--verify", "--end-of-options", s.BaseRef+"^{commit}")
		if err != nil {
			return Collection{}, fmt.Errorf("base ref for %s: %w", s.Name, err)
		}
		c.Members = append(c.Members, Member{MemberSpec: s, Path: filepath.Join(c.Root, s.Name), BaseCommit: commit, CommonDir: m.common[r.ID]})
	}
	return c, nil
}

func (m *Manager) provision(ctx context.Context, c Collection) (Collection, error) {
	c.State, c.Error = "creating", ""
	fail := func(err error) (Collection, error) {
		c.State, c.Error = "failed", err.Error()
		return clone(c), errors.Join(err, m.save(c))
	}
	if err := os.MkdirAll(c.Root, 0700); err != nil {
		return fail(err)
	}
	resolved, err := filepath.EvalSymlinks(c.Root)
	if err != nil || resolved != c.Root {
		return fail(errors.New("collection root moved or replaced by a symlink"))
	}
	for i := range c.Members {
		member := &c.Members[i]
		r, ok := m.repos[member.RepositoryID]
		if !ok || m.common[r.ID] != member.CommonDir {
			return fail(errors.New("collection source repository unavailable or changed"))
		}
		_, err := os.Lstat(member.Path)
		if errors.Is(err, os.ErrNotExist) {
			if member.Ready {
				return fail(errors.New("previously provisioned worktree is missing; refusing to recreate it"))
			}
			_, err = m.git(ctx, r.Path, "worktree", "add", "-b", member.Branch, "--", member.Path, member.BaseCommit)
		}
		if err != nil {
			return fail(err)
		}
		if err = m.validate(ctx, *member); err != nil {
			return fail(err)
		}
		if !member.Ready {
			head, err := m.git(ctx, member.Path, "rev-parse", "HEAD")
			if err != nil || head != member.BaseCommit {
				return fail(errors.New("unfinished worktree no longer matches its pinned base; preserve it for inspection"))
			}
			branch, err := m.git(ctx, member.Path, "symbolic-ref", "--short", "HEAD")
			if err != nil || branch != member.Branch {
				return fail(errors.New("unfinished worktree has a different branch; preserve it for inspection"))
			}
		}
		member.Ready = true
		if err = m.save(c); err != nil {
			return fail(err)
		}
	}
	c.State = "ready"
	if err := m.save(c); err != nil {
		return clone(c), err
	}
	return clone(c), nil
}

func (m *Manager) validate(ctx context.Context, member Member) error {
	path, err := filepath.EvalSymlinks(member.Path)
	if err != nil || path != member.Path {
		return errors.New("worktree moved or replaced by a symlink")
	}
	top, err := m.git(ctx, member.Path, "rev-parse", "--show-toplevel")
	if err != nil || top != member.Path {
		return errors.New("member is not the expected worktree root")
	}
	common, err := m.git(ctx, member.Path, "rev-parse", "--path-format=absolute", "--git-common-dir")
	if err != nil {
		return err
	}
	common, err = filepath.EvalSymlinks(common)
	if err != nil || common != member.CommonDir {
		return errors.New("worktree belongs to a different repository")
	}
	// Verify Git's registration too, rather than trusting a copied .git file.
	gitdir, err := m.git(ctx, member.Path, "rev-parse", "--absolute-git-dir")
	if err != nil {
		return err
	}
	link, err := os.ReadFile(filepath.Join(gitdir, "gitdir"))
	if err != nil || strings.TrimSpace(string(link)) != filepath.Join(member.Path, ".git") {
		return errors.New("worktree registration does not match its path")
	}
	return nil
}

// Acquire records one of the workspace's sessions. Active must include both
// its agent and surviving managed commands. It is called under the manager lock
// and must not call the manager. Failed startups must Release their reservation.
func (m *Manager) Acquire(ctx context.Context, id, owner string, active func(string) bool) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return Collection{}, os.ErrNotExist
	}
	if m.closed || c.State != "ready" || owner == "" {
		return Collection{}, errors.New("collection is not available for attachment")
	}
	if c.OwnershipUncertain {
		return Collection{}, errors.New("collection has unreconciled ownership from a previous Runner; verify its agent and commands have stopped before reconciliation")
	}
	c = clone(c)
	owners := []string{}
	found := false
	for _, previous := range c.OwnerExecutionIDs {
		if active(previous) {
			owners = append(owners, previous)
			found = found || previous == owner
		}
	}
	if !found {
		owners = append(owners, owner)
	}
	if len(owners) > 64 {
		return Collection{}, errors.New("workspace session capacity exceeded")
	}
	for _, member := range c.Members {
		if m.common[member.RepositoryID] != member.CommonDir {
			return Collection{}, errors.New("collection source repository unavailable or changed")
		}
		if err := m.validate(ctx, member); err != nil {
			return Collection{}, err
		}
	}
	c.OwnerExecutionIDs = owners
	c.syncOwners()
	if err := m.save(c); err != nil {
		return Collection{}, err
	}
	return clone(c), nil
}
func (m *Manager) Release(id, owner string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return errors.New("collection manager closed")
	}
	c, ok := m.items[id]
	if !ok {
		return os.ErrNotExist
	}
	if c.OwnershipUncertain {
		return nil
	}
	c = clone(c)
	owners := []string{}
	for _, value := range c.OwnerExecutionIDs {
		if value != owner {
			owners = append(owners, value)
		}
	}
	c.OwnerExecutionIDs = owners
	c.syncOwners()
	return m.save(c)
}
func (m *Manager) Archive(id string, active func(string) bool) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return Collection{}, os.ErrNotExist
	}
	if m.closed {
		return Collection{}, errors.New("collection manager closed")
	}
	if c.OwnershipUncertain {
		return Collection{}, errors.New("workspace ownership must be reconciled")
	}
	for _, owner := range c.OwnerExecutionIDs {
		if active(owner) {
			return Collection{}, errors.New("cannot archive a collection with an active agent or command")
		}
	}
	c.State = "archived"
	c.OwnerExecutionIDs = nil
	c.syncOwners()
	if err := m.save(c); err != nil {
		return Collection{}, err
	}
	return clone(c), nil
}

// Reconcile is explicit operator attestation, not process detection. It only
// resolves a previous Runner's uncertain owner, with an exact compare-and-set.
// The caller must establish that both the agent and its commands have stopped.
func (m *Manager) Reconcile(id, expectedOwner, reason string, processesStopped bool) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return Collection{}, os.ErrNotExist
	}
	if m.closed || !c.OwnershipUncertain || expectedOwner == "" || c.OwnerExecutionID != expectedOwner {
		return Collection{}, errors.New("uncertain collection owner does not match")
	}
	if !processesStopped || strings.TrimSpace(reason) == "" || len(reason) > 1024 {
		return Collection{}, errors.New("explicit stopped-process attestation and reconciliation reason required")
	}
	c.LastOwnershipResolution = expectedOwner + ": " + reason
	c = clone(c)
	c.OwnerExecutionIDs = c.OwnerExecutionIDs[1:]
	m.settleLocked(&c)
	if err := m.save(c); err != nil {
		return Collection{}, err
	}
	return clone(c), nil
}

// Adopted records that owner's live agent was re-attached by this Runner. Its
// ownership is certain again; previous owners nobody adopted stay uncertain
// and are ordered first, which is the owner Reconcile resolves next.
func (m *Manager) Adopted(id, owner string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return os.ErrNotExist
	}
	if m.closed {
		return errors.New("collection manager closed")
	}
	if m.adopted == nil {
		m.adopted = map[string]bool{}
	}
	m.adopted[owner] = true
	if !c.OwnershipUncertain {
		return nil
	}
	c = clone(c)
	m.settleLocked(&c)
	return m.save(c)
}

// settleLocked orders unadopted owners first and keeps ownership uncertain only
// while one of them remains.
func (m *Manager) settleLocked(c *Collection) {
	var uncertain, certain []string
	for _, owner := range c.OwnerExecutionIDs {
		if m.adopted[owner] {
			certain = append(certain, owner)
		} else {
			uncertain = append(uncertain, owner)
		}
	}
	c.OwnerExecutionIDs = append(uncertain, certain...)
	c.OwnershipUncertain = len(uncertain) > 0
	c.syncOwners()
}

func (m *Manager) Close() error {
	m.cancel()
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return nil
	}
	m.closed = true
	return m.lock.Close()
}

func (m *Manager) save(c Collection) error {
	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return err
	}
	if len(data) > 1<<20 {
		return errors.New("collection record exceeds 1 MiB")
	}
	dir := filepath.Join(m.root, ".lazybox-collections", "records")
	f, err := os.CreateTemp(dir, ".record-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err = errors.Join(err, closeErr); err != nil {
		return err
	}
	if err = os.Rename(f.Name(), filepath.Join(dir, c.ID+".json")); err != nil {
		return err
	}
	m.items[c.ID] = clone(c)
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer d.Close()
	return d.Sync()
}

// capOutput bounds subprocess output without stopping the pipe reader.
type capOutput struct{ data []byte }

func (b *capOutput) Write(p []byte) (int, error) {
	n := len(p)
	const limit = 64 << 10
	if len(p) >= limit {
		b.data = append(b.data[:0], p[len(p)-limit:]...)
	} else {
		if len(b.data)+len(p) > limit {
			b.data = b.data[len(b.data)+len(p)-limit:]
		}
		b.data = append(b.data, p...)
	}
	return n, nil
}
func (m *Manager) git(ctx context.Context, path string, args ...string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 2*time.Minute)
	defer cancel()
	stop := context.AfterFunc(m.ctx, cancel)
	defer stop()
	cmd := exec.CommandContext(ctx, "git", append([]string{"-C", path}, args...)...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = time.Second
	for _, e := range os.Environ() {
		key, _, _ := strings.Cut(e, "=")
		if strings.HasPrefix(key, "GIT_") {
			continue
		}
		cmd.Env = append(cmd.Env, e)
	}
	cmd.Env = append(cmd.Env, "GIT_TERMINAL_PROMPT=0")
	var output capOutput
	cmd.Stdout, cmd.Stderr = &output, &output
	err := cmd.Run()
	text := strings.TrimSpace(string(output.data))
	if err != nil {
		return "", fmt.Errorf("git %s: %w: %s", args[0], errors.Join(err, ctx.Err()), text)
	}
	return text, nil
}
func (m *Manager) branchExists(ctx context.Context, repo, branch string) (bool, error) {
	// for-each-ref exits successfully for an absent ref, so genuine Git errors
	// cannot be mistaken for permission to create a branch.
	refs, err := m.git(ctx, repo, "for-each-ref", "--format=%(refname)", "refs/heads/"+branch)
	if err != nil {
		return false, err
	}
	for _, ref := range strings.Split(refs, "\n") {
		if ref == "refs/heads/"+branch {
			return true, nil
		}
	}
	return false, nil
}

func folderName(name string) string {
	parts := strings.Fields(strings.ToLower(strings.TrimSpace(name)))
	return strings.Join(parts, "-")
}
func validRoot(root string, c Collection) bool {
	if c.Version >= 4 {
		return component.MatchString(c.Slug) && c.Root == filepath.Join(root, c.Slug)
	}
	if c.Version == 1 || c.LegacyRoot {
		return c.Root == filepath.Join(root, c.ID)
	}
	name := folderName(c.Name)
	return component.MatchString(name) && c.Root == filepath.Join(root, name)
}

// Rename edits presentation only. Slug, paths, branches, retry identity and owners stay fixed.
func (m *Manager) Rename(id, name string) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return Collection{}, os.ErrNotExist
	}
	name = strings.TrimSpace(name)
	if m.closed || name == "" || len(name) > 128 || strings.IndexFunc(name, unicode.IsControl) >= 0 {
		return Collection{}, errors.New("valid workspace name required")
	}
	c = clone(c)
	c.Name = name
	c.syncOwners()
	if err := m.save(c); err != nil {
		return Collection{}, err
	}
	return clone(c), nil
}
