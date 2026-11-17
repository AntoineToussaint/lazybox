package codecheckpoints

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

// RefPrefix is the private namespace every checkpoint ref lives under.
const RefPrefix = "refs/lazybox/checkpoints/"

// Repository is one workspace member a checkpoint covers.
type Repository struct{ Member, Path string }

// RepoPoint is one repository's files at a checkpoint.
type RepoPoint struct {
	Member       string   `json:"member"`
	Path         string   `json:"path"`
	Commit       string   `json:"commit,omitempty"`
	Tree         string   `json:"tree,omitempty"`
	Head         string   `json:"head,omitempty"`
	Branch       string   `json:"branch,omitempty"`
	Skipped      []string `json:"skipped,omitempty"`
	SkippedCount int      `json:"skippedCount,omitempty"`
	// Error is why this repository has no checkpoint here.
	Error string `json:"error,omitempty"`
}

func (r RepoPoint) ok() bool { return r.Error == "" && r.Commit != "" && r.Tree != "" }

// Point is a checkpoint: the set of per-repository commits taken together.
type Point struct {
	Ref   string      `json:"ref"`
	At    time.Time   `json:"at"`
	Repos []RepoPoint `json:"repos"`
}

func (p Point) repo(member string) (RepoPoint, bool) {
	for _, r := range p.Repos {
		if r.Member == member {
			return r, true
		}
	}
	return RepoPoint{}, false
}

// Turn is one agent turn: its files before, and after once it ended.
type Turn struct {
	Seq         int    `json:"seq"`
	OperationID string `json:"operationId"`
	Prompt      string `json:"prompt,omitempty"`
	Before      Point  `json:"before"`
	After       *Point `json:"after,omitempty"`
	// What the turn changed, before → after, across repositories.
	Files     int `json:"files"`
	Additions int `json:"additions"`
	Deletions int `json:"deletions"`
}

// Revert is one restore of a workspace's files: to before a turn, or back to
// how they were before an earlier revert (an undo). Restoring Safety undoes it.
type Revert struct {
	ID       string    `json:"id"`
	Kind     string    `json:"kind"`
	Turn     int       `json:"turn"`
	UndoOf   string    `json:"undoOf,omitempty"`
	UndoneBy string    `json:"undoneBy,omitempty"`
	At       time.Time `json:"at"`
	Safety   Point     `json:"safety"`
	Target   Point     `json:"target"`
	Files    int       `json:"files"`
	Problems []string  `json:"problems,omitempty"`
}

// Record is everything a conversation's checkpoints hold.
type Record struct {
	Conversation string   `json:"conversation"`
	Turns        []Turn   `json:"turns"`
	Reverts      []Revert `json:"reverts"`
	NextTurn     int      `json:"nextTurn"`
	NextRevert   int      `json:"nextRevert"`
	// Note is what the agent is told with the next message after a revert.
	Note string `json:"note,omitempty"`
	// Last is each member's latest checkpoint, reused while nothing changed.
	Last map[string]RepoPoint `json:"last,omitempty"`
}

func (r *Record) turn(seq int) (*Turn, error) {
	for i := range r.Turns {
		if r.Turns[i].Seq == seq {
			return &r.Turns[i], nil
		}
	}
	return nil, ErrNoCheckpoint
}

// ErrNoCheckpoint is a turn or revert this conversation has no checkpoint for
// (never taken, or pruned).
var ErrNoCheckpoint = errors.New("no checkpoint for that turn; it was never taken or is older than the ones kept")

// ErrNothingToRevert is a revert to files that already match the checkpoint.
var ErrNothingToRevert = errors.New("the files already match that checkpoint; there is nothing to revert")

// Manager keeps the checkpoints of every conversation on this Runner.
type Manager struct {
	root   string
	limits Limits
	mu     sync.Mutex
	convs  map[string]*sync.Mutex
	repos  map[string]*sync.Mutex
}

// Open keeps conversation records under root (in the Runner's state).
func Open(root string, limits Limits) (*Manager, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("checkpoint state must be an absolute directory")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	// Private indexes left by a Runner that stopped mid-checkpoint.
	_ = os.RemoveAll(filepath.Join(root, "tmp"))
	if err := os.MkdirAll(filepath.Join(root, "tmp"), 0700); err != nil {
		return nil, err
	}
	return &Manager{root: root, limits: limits.withDefaults(), convs: map[string]*sync.Mutex{}, repos: map[string]*sync.Mutex{}}, nil
}

var validID = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$`)

func (m *Manager) lockConversation(id string) (func(), error) {
	if !validID.MatchString(id) {
		return nil, errors.New("invalid conversation id")
	}
	m.mu.Lock()
	l := m.convs[id]
	if l == nil {
		l = &sync.Mutex{}
		m.convs[id] = l
	}
	m.mu.Unlock()
	l.Lock()
	return l.Unlock, nil
}

// lockRepositories serializes everything that reads or writes these work
// trees' checkpoints, across conversations sharing a repository.
func (m *Manager) lockRepositories(paths []string) func() {
	paths = append([]string(nil), paths...)
	sort.Strings(paths)
	locks := []*sync.Mutex{}
	m.mu.Lock()
	seen := map[string]bool{}
	for _, p := range paths {
		if seen[p] {
			continue
		}
		seen[p] = true
		l := m.repos[p]
		if l == nil {
			l = &sync.Mutex{}
			m.repos[p] = l
		}
		locks = append(locks, l)
	}
	m.mu.Unlock()
	for _, l := range locks {
		l.Lock()
	}
	return func() {
		for i := len(locks) - 1; i >= 0; i-- {
			locks[i].Unlock()
		}
	}
}

func (m *Manager) file(id string) string { return filepath.Join(m.root, id+".json") }

func (m *Manager) load(id string) (*Record, error) {
	data, err := os.ReadFile(m.file(id))
	if errors.Is(err, os.ErrNotExist) {
		return &Record{Conversation: id, Turns: []Turn{}, Reverts: []Revert{}, Last: map[string]RepoPoint{}}, nil
	}
	if err != nil {
		return nil, err
	}
	r := &Record{}
	if err = json.Unmarshal(data, r); err != nil {
		return nil, err
	}
	if r.Last == nil {
		r.Last = map[string]RepoPoint{}
	}
	return r, nil
}

func (m *Manager) save(r *Record) error {
	data, err := json.Marshal(r)
	if err != nil {
		return err
	}
	tmp, err := os.CreateTemp(m.root, "."+r.Conversation+"-*.json")
	if err != nil {
		return err
	}
	if _, err = tmp.Write(data); err == nil {
		err = tmp.Sync()
	}
	if closeErr := tmp.Close(); err == nil {
		err = closeErr
	}
	if err == nil {
		err = os.Rename(tmp.Name(), m.file(r.Conversation))
	}
	if err != nil {
		_ = os.Remove(tmp.Name())
	}
	return err
}

func ref(conversation, name string) string { return RefPrefix + conversation + "/" + name }

func paths(repos []Repository) []string {
	out := make([]string, 0, len(repos))
	for _, r := range repos {
		out = append(out, r.Path)
	}
	return out
}

func pointPaths(p Point) []string {
	out := []string{}
	for _, r := range p.Repos {
		if r.Path != "" {
			out = append(out, r.Path)
		}
	}
	return out
}

// take checkpoints every repository under ref name. The caller holds the
// conversation and repository locks. A repository that cannot be read gets
// an error in its place; the others are still recorded.
func (m *Manager) take(ctx context.Context, rec *Record, repos []Repository, name, message string) Point {
	p := Point{Ref: name, At: time.Now().UTC(), Repos: []RepoPoint{}}
	for _, repo := range repos {
		p.Repos = append(p.Repos, m.takeOne(ctx, rec, repo, name, message))
	}
	return p
}

func (m *Manager) takeOne(ctx context.Context, rec *Record, repo Repository, name, message string) RepoPoint {
	point := RepoPoint{Member: repo.Member, Path: repo.Path}
	w, err := snapshot(ctx, repo.Path, filepath.Join(m.root, "tmp"), m.limits)
	if err != nil {
		point.Error = err.Error()
		return point
	}
	point.Path, point.Tree, point.Head, point.Branch, point.Skipped, point.SkippedCount = w.Top, w.Tree, w.Head, w.Branch, w.Skipped, w.SkippedCount
	// Nothing changed since the last checkpoint: no new objects, the same commit.
	if last, ok := rec.Last[repo.Member]; ok && last.ok() && last.Path == w.Top && last.Tree == w.Tree && last.Head == w.Head {
		point.Commit = last.Commit
	} else if point.Commit, err = commit(ctx, w.Top, w, message); err != nil {
		point.Error = err.Error()
		return point
	}
	if _, err = git(ctx, w.Top, "update-ref", ref(rec.Conversation, name), point.Commit); err != nil {
		point.Error = err.Error()
		return point
	}
	rec.Last[repo.Member] = point
	return point
}

// summarize counts what a finished turn changed.
func summarize(ctx context.Context, t *Turn) {
	t.Files, t.Additions, t.Deletions = 0, 0, 0
	if t.After == nil {
		return
	}
	for _, before := range t.Before.Repos {
		after, ok := t.After.repo(before.Member)
		if !before.ok() || !ok || !after.ok() || before.Tree == after.Tree {
			continue
		}
		files, _, err := changes(ctx, before.Path, before.Commit, after.Commit)
		if err != nil {
			continue
		}
		t.Files += len(files)
		for _, f := range files {
			t.Additions += f.Additions
			t.Deletions += f.Deletions
		}
	}
}

func excerpt(text string) string {
	line := strings.TrimSpace(strings.SplitN(strings.TrimSpace(text), "\n", 2)[0])
	if utf8.RuneCountInString(line) > 80 {
		line = string([]rune(line)[:79]) + "…"
	}
	return line
}

// BeginTurn checkpoints repos before the turn that operation starts. A turn
// still open is closed at this checkpoint: nothing ran in between.
func (m *Manager) BeginTurn(ctx context.Context, conversation string, repos []Repository, operation, prompt string) (Turn, error) {
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return Turn{}, err
	}
	defer unlock()
	rec, err := m.load(conversation)
	if err != nil {
		return Turn{}, err
	}
	release := m.lockRepositories(paths(repos))
	seq := rec.NextTurn + 1
	point := m.take(ctx, rec, repos, fmt.Sprintf("turn-%d-before", seq), fmt.Sprintf("Lazybox checkpoint: before turn %d\n\nConversation %s", seq, conversation))
	if n := len(rec.Turns); n > 0 && rec.Turns[n-1].After == nil {
		closed := point
		rec.Turns[n-1].After = &closed
		summarize(ctx, &rec.Turns[n-1])
	}
	release()
	t := Turn{Seq: seq, OperationID: operation, Prompt: excerpt(prompt), Before: point}
	rec.NextTurn = seq
	rec.Turns = append(rec.Turns, t)
	m.prune(ctx, rec)
	return t, m.save(rec)
}

// EndTurn checkpoints repos after the turn operation started, once it ended.
// Nothing is done when a later turn already began (and closed it at its own
// start). operation "" is a turn whose start this Runner did not see: the
// open turn, if it began before ended.
func (m *Manager) EndTurn(ctx context.Context, conversation string, repos []Repository, operation string, ended time.Time) error {
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return err
	}
	defer unlock()
	rec, err := m.load(conversation)
	if err != nil {
		return err
	}
	n := len(rec.Turns)
	if n == 0 || rec.Turns[n-1].After != nil {
		return nil
	}
	t := &rec.Turns[n-1]
	if (operation != "" && t.OperationID != operation) || (operation == "" && t.Before.At.After(ended)) {
		return nil
	}
	release := m.lockRepositories(paths(repos))
	after := m.take(ctx, rec, repos, fmt.Sprintf("turn-%d-after", t.Seq), fmt.Sprintf("Lazybox checkpoint: after turn %d\n\nConversation %s", t.Seq, conversation))
	t.After = &after
	summarize(ctx, t)
	release()
	return m.save(rec)
}

// deleteRefs removes the named refs of a conversation from every repository
// they may be in. A repository that is gone has nothing left to remove.
func deleteRefs(ctx context.Context, conversation string, repoPaths []string, names []string) {
	seen := map[string]bool{}
	for _, path := range repoPaths {
		if seen[path] {
			continue
		}
		seen[path] = true
		if _, err := os.Stat(path); err != nil {
			continue
		}
		var batch bytes.Buffer
		for _, name := range names {
			batch.WriteString("delete " + ref(conversation, name) + "\n")
		}
		_, _, _ = gitCall{dir: path, stdin: batch.Bytes()}.run(ctx, "update-ref", "--stdin")
	}
}

// prune keeps the newest turns and reverts within bounds, deleting the refs
// of the ones dropped so Git may collect what nothing else references.
func (m *Manager) prune(ctx context.Context, rec *Record) {
	for len(rec.Turns) > m.limits.KeepTurns {
		t := rec.Turns[0]
		rec.Turns = rec.Turns[1:]
		repoPaths := pointPaths(t.Before)
		if t.After != nil {
			repoPaths = append(repoPaths, pointPaths(*t.After)...)
		}
		deleteRefs(ctx, rec.Conversation, repoPaths, []string{fmt.Sprintf("turn-%d-before", t.Seq), fmt.Sprintf("turn-%d-after", t.Seq)})
	}
	for len(rec.Reverts) > m.limits.KeepReverts {
		r := rec.Reverts[0]
		rec.Reverts = rec.Reverts[1:]
		deleteRefs(ctx, rec.Conversation, pointPaths(r.Safety), []string{r.Safety.Ref})
	}
}

// List returns a conversation's checkpoints, oldest turn first.
func (m *Manager) List(conversation string) (Record, error) {
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return Record{}, err
	}
	defer unlock()
	rec, err := m.load(conversation)
	if err != nil {
		return Record{}, err
	}
	return *rec, nil
}

// RepoChanges is what changed in one repository.
type RepoChanges struct {
	Member    string
	Files     []FileChange
	Truncated bool
	Error     string
}

// live is the current files of a repository as a tree, without recording it.
func (m *Manager) live(ctx context.Context, path string) (workTree, error) {
	return snapshot(ctx, path, filepath.Join(m.root, "tmp"), m.limits)
}

// span is the two sides of a turn per repository: before, and after or (for
// a turn still running) the files as they are now.
func (m *Manager) span(ctx context.Context, t Turn, member string) (path, from, to string, err error) {
	before, ok := t.Before.repo(member)
	if !ok || !before.ok() {
		if ok && before.Error != "" {
			return "", "", "", errors.New(before.Error)
		}
		return "", "", "", errors.New("this repository has no checkpoint for that turn")
	}
	if t.After != nil {
		after, ok := t.After.repo(member)
		if !ok || !after.ok() {
			return "", "", "", errors.New("this repository has no checkpoint after that turn")
		}
		return before.Path, before.Commit, after.Commit, nil
	}
	now, err := m.live(ctx, before.Path)
	if err != nil {
		return "", "", "", err
	}
	return before.Path, before.Commit, now.Tree, nil
}

// Changes lists what a turn changed in each repository. For a turn still
// running it is what changed so far; inProgress says so.
func (m *Manager) Changes(ctx context.Context, conversation string, seq int) (repos []RepoChanges, inProgress bool, err error) {
	rec, err := m.List(conversation)
	if err != nil {
		return nil, false, err
	}
	t, err := rec.turn(seq)
	if err != nil {
		return nil, false, err
	}
	release := m.lockRepositories(pointPaths(t.Before))
	defer release()
	for _, before := range t.Before.Repos {
		row := RepoChanges{Member: before.Member, Files: []FileChange{}}
		path, from, to, err := m.span(ctx, *t, before.Member)
		if err == nil {
			row.Files, row.Truncated, err = changes(ctx, path, from, to)
		}
		if err != nil {
			row.Error = err.Error()
		}
		repos = append(repos, row)
	}
	return repos, t.After == nil, nil
}

// Diff is one file's change in a turn.
func (m *Manager) Diff(ctx context.Context, conversation string, seq int, member, path, original string) (Diff, error) {
	rec, err := m.List(conversation)
	if err != nil {
		return Diff{}, err
	}
	t, err := rec.turn(seq)
	if err != nil {
		return Diff{}, err
	}
	release := m.lockRepositories(pointPaths(t.Before))
	defer release()
	top, from, to, err := m.span(ctx, *t, member)
	if err != nil {
		return Diff{}, err
	}
	return fileDiff(ctx, top, from, to, path, original)
}

// RepoPreview is what reverting would do to one repository.
type RepoPreview struct {
	Member string
	// Files differ now from the checkpoint, from the revert's side: A is
	// recreated, D is deleted, M is put back as it was.
	Files     []FileChange
	Truncated bool
	// CommitsSince counts commits HEAD gained since the checkpoint. They stay
	// on the branch; reverting shows as uncommitted changes against them.
	CommitsSince int
	// BranchThen differs from BranchNow when HEAD moved to another branch.
	BranchThen, BranchNow string
	Error                 string
}

// Preview says what reverting to before turn seq would change, without
// changing anything. current is the workspace's repositories now: one with
// no checkpoint at that turn is reported and left as it is.
func (m *Manager) Preview(ctx context.Context, conversation string, seq int, current []Repository) ([]RepoPreview, error) {
	rec, err := m.List(conversation)
	if err != nil {
		return nil, err
	}
	t, err := rec.turn(seq)
	if err != nil {
		return nil, err
	}
	return m.preview(ctx, t.Before, current), nil
}

func (m *Manager) preview(ctx context.Context, target Point, current []Repository) []RepoPreview {
	release := m.lockRepositories(append(pointPaths(target), paths(current)...))
	defer release()
	out := []RepoPreview{}
	for _, point := range target.Repos {
		row := RepoPreview{Member: point.Member, Files: []FileChange{}, BranchThen: point.Branch}
		if !point.ok() {
			row.Error = "no checkpoint for this repository at that turn: " + point.Error
			out = append(out, row)
			continue
		}
		now, err := m.live(ctx, point.Path)
		if err == nil {
			row.BranchNow = now.Branch
			row.Files, row.Truncated, err = changes(ctx, point.Path, now.Tree, point.Commit)
			if point.Head != "" && now.Head != "" && point.Head != now.Head {
				if count, err := git(ctx, point.Path, "rev-list", "--count", point.Head+".."+now.Head); err == nil {
					row.CommitsSince, _ = strconv.Atoi(count)
				}
			}
		}
		if err != nil {
			row.Error = err.Error()
		}
		out = append(out, row)
	}
	for _, repo := range current {
		if _, ok := target.repo(repo.Member); !ok {
			out = append(out, RepoPreview{Member: repo.Member, Files: []FileChange{}, Error: "added to the workspace after that turn; left as it is"})
		}
	}
	return out
}

// Revert puts every repository's files back as they were before turn seq.
// guard runs once the conversation is locked and refuses while an agent is
// working. The current files are saved first: undoing restores them.
func (m *Manager) Revert(ctx context.Context, conversation string, seq int, guard func() error) (Revert, error) {
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return Revert{}, err
	}
	defer unlock()
	rec, err := m.load(conversation)
	if err != nil {
		return Revert{}, err
	}
	t, err := rec.turn(seq)
	if err != nil {
		return Revert{}, err
	}
	r, err := m.restorePoint(ctx, rec, t.Before, guard)
	if err != nil {
		return Revert{}, err
	}
	r.Kind, r.Turn = "revert", seq
	what := "turn " + strconv.Itoa(seq)
	if t.Prompt != "" {
		what += " (“" + t.Prompt + "”)"
	}
	rec.Note = "[Lazybox] The person reverted the workspace's files to how they were before " + what + ". Edits made from that turn on are no longer on disk; branch history was not changed, so any commits made since remain and the revert shows as uncommitted changes. Re-read files before relying on what you remember of them."
	return m.record(ctx, rec, r)
}

// Undo restores the files a revert replaced. Only the latest revert can be
// undone; an undo is itself a revert, so it can be undone in turn.
func (m *Manager) Undo(ctx context.Context, conversation, id string, guard func() error) (Revert, error) {
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return Revert{}, err
	}
	defer unlock()
	rec, err := m.load(conversation)
	if err != nil {
		return Revert{}, err
	}
	n := len(rec.Reverts)
	if n == 0 || rec.Reverts[n-1].ID != id {
		for _, r := range rec.Reverts {
			if r.ID == id {
				return Revert{}, errors.New("only the latest revert can be undone")
			}
		}
		return Revert{}, ErrNoCheckpoint
	}
	undone := &rec.Reverts[n-1]
	if undone.UndoneBy != "" {
		return Revert{}, errors.New("that revert was already undone")
	}
	r, err := m.restorePoint(ctx, rec, undone.Safety, guard)
	if err != nil {
		return Revert{}, err
	}
	r.Kind, r.Turn, r.UndoOf = "undo", undone.Turn, undone.ID
	undone = &rec.Reverts[n-1]
	undone.UndoneBy = r.ID
	rec.Note = "[Lazybox] The person undid their last revert: the workspace's files are back as they were just before it. Re-read files before relying on what you remember of them."
	return m.record(ctx, rec, r)
}

func (m *Manager) record(ctx context.Context, rec *Record, r Revert) (Revert, error) {
	// The undone revert is updated in place; append after reading it.
	rec.Reverts = append(rec.Reverts, r)
	m.prune(ctx, rec)
	return r, m.save(rec)
}

// restorePoint saves the current files as a safety checkpoint, then restores
// target in every repository it covers, and checks the result.
func (m *Manager) restorePoint(ctx context.Context, rec *Record, target Point, guard func() error) (Revert, error) {
	release := m.lockRepositories(pointPaths(target))
	defer release()
	if guard != nil {
		if err := guard(); err != nil {
			return Revert{}, err
		}
	}
	id := rec.NextRevert + 1
	r := Revert{ID: "r" + strconv.Itoa(id), At: time.Now().UTC(), Target: target, Problems: []string{}}
	repos := []Repository{}
	for _, point := range target.Repos {
		if point.ok() {
			repos = append(repos, Repository{Member: point.Member, Path: point.Path})
		} else {
			r.Problems = append(r.Problems, point.Member+": no checkpoint; left as it is")
		}
	}
	if len(repos) == 0 {
		return Revert{}, errors.New("no repository has a checkpoint to restore")
	}
	name := fmt.Sprintf("revert-%d-safety", id)
	r.Safety = m.take(ctx, rec, repos, name, fmt.Sprintf("Lazybox checkpoint: before revert %d\n\nConversation %s", id, rec.Conversation))
	for _, safety := range r.Safety.Repos {
		if !safety.ok() {
			// Without a safety checkpoint the revert could not be undone.
			r.Problems = append(r.Problems, safety.Member+": current files could not be saved, so it was left as it is: "+safety.Error)
		}
	}
	saved, differs := false, false
	for _, safety := range r.Safety.Repos {
		if safety.ok() {
			saved = true
			if point, _ := target.repo(safety.Member); point.Tree != safety.Tree {
				differs = true
			}
		}
	}
	if !saved {
		return Revert{}, errors.New("the current files could not be saved first, so nothing was reverted: " + strings.Join(r.Problems, "; "))
	}
	if !differs {
		return Revert{}, ErrNothingToRevert
	}
	rec.NextRevert = id
	scratch := filepath.Join(m.root, "tmp")
	for _, safety := range r.Safety.Repos {
		if !safety.ok() {
			continue
		}
		point, _ := target.repo(safety.Member)
		changed, problems, err := restore(ctx, safety.Path, scratch, safety.Tree, point.Tree)
		r.Files += len(changed)
		for _, p := range problems {
			r.Problems = append(r.Problems, safety.Member+": "+p)
		}
		if err != nil {
			r.Problems = append(r.Problems, safety.Member+": "+err.Error())
			continue
		}
		// What is on disk now is what was asked for, or the person hears which files are not.
		now, err := m.live(ctx, safety.Path)
		if err != nil {
			r.Problems = append(r.Problems, safety.Member+": could not check the result: "+err.Error())
			continue
		}
		if now.Tree != point.Tree {
			left, _ := rawDiff(ctx, safety.Path, point.Tree, now.Tree)
			for i, e := range left {
				if i == skippedListed {
					r.Problems = append(r.Problems, fmt.Sprintf("%s: %d more files differ", safety.Member, len(left)-i))
					break
				}
				r.Problems = append(r.Problems, safety.Member+": "+e.Path+" differs from the checkpoint")
			}
		}
	}
	return r, nil
}

// Note is what the agent should be told with its next message, or "".
func (m *Manager) Note(conversation string) string {
	rec, err := m.List(conversation)
	if err != nil {
		return ""
	}
	return rec.Note
}

// NoteDelivered forgets note once the agent was sent it.
func (m *Manager) NoteDelivered(conversation, note string) error {
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return err
	}
	defer unlock()
	rec, err := m.load(conversation)
	if err != nil || rec.Note != note {
		return err
	}
	rec.Note = ""
	return m.save(rec)
}

// Delete removes a conversation's checkpoints: its record and every ref.
func (m *Manager) Delete(ctx context.Context, conversation string) error {
	if !validID.MatchString(conversation) {
		return nil // nothing is ever recorded under such an id
	}
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return err
	}
	defer unlock()
	rec, err := m.load(conversation)
	if err != nil {
		return err
	}
	repoPaths := []string{}
	for _, t := range rec.Turns {
		repoPaths = append(repoPaths, pointPaths(t.Before)...)
		if t.After != nil {
			repoPaths = append(repoPaths, pointPaths(*t.After)...)
		}
	}
	for _, r := range rec.Reverts {
		repoPaths = append(repoPaths, pointPaths(r.Safety)...)
	}
	for _, p := range rec.Last {
		repoPaths = append(repoPaths, p.Path)
	}
	seen := map[string]bool{}
	for _, path := range repoPaths {
		if seen[path] {
			continue
		}
		seen[path] = true
		if _, err := os.Stat(path); err != nil {
			continue
		}
		out, err := git(ctx, path, "for-each-ref", "--format=%(refname)", RefPrefix+conversation+"/")
		if err != nil || out == "" {
			continue
		}
		var batch bytes.Buffer
		for _, name := range strings.Split(out, "\n") {
			batch.WriteString("delete " + name + "\n")
		}
		if _, _, err = (gitCall{dir: path, stdin: batch.Bytes()}).run(ctx, "update-ref", "--stdin"); err != nil {
			return err
		}
	}
	if err = os.Remove(m.file(conversation)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}
