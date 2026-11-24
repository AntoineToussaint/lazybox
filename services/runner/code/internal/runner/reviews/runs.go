package reviews

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
)

// An agent review (GITHUB.md, "Agent review"): one reviewer conversation,
// read-only, started on a scope of another conversation's work. Its findings
// are comments of the workspace's review written by the agent (Author is
// AuthorAgent), so they share the store, the anchoring and the freshness of a
// person's comments. A run is kept in the workspace's review file, so it
// survives Runner restarts with its findings.

// Run states.
const (
	RunRunning = "running"
	RunDone    = "done"
	RunFailed  = "failed"
	// RunCancelled: the person cancelled the review (CancelReview).
	RunCancelled = "cancelled"
	// RunStopped: the person stopped or ended the reviewer conversation by
	// hand (Stop, End session, Delete) while it reviewed.
	RunStopped = "stopped"
)

// Scopes a review covers.
const (
	ScopeChanges = "changes"
	ScopeTurn    = "turn"
	ScopePR      = "pr"
)

// ReviewInput identifies the exact tree and comparison behind a template result.
type ReviewInput struct {
	Member  string `json:"member"`
	Base    string `json:"base"`
	Commit  string `json:"commit"`
	Head    string `json:"head"`
	Skipped int    `json:"skipped,omitempty"`
}

// Run is one agent review.
type Run struct {
	Verification    []VerificationCheck `json:"verification,omitempty"`
	ContractDigest  string              `json:"contractDigest,omitempty"`
	ReviewAttempt   string              `json:"reviewAttempt,omitempty"`
	ReviewWorkspace string              `json:"reviewWorkspace,omitempty"`
	MemberHeads     map[string]string   `json:"memberHeads,omitempty"`

	ID             string        `json:"id"`
	TemplateID     string        `json:"templateId,omitempty"`
	Input          []ReviewInput `json:"input,omitempty"`
	PRHandoffSent  bool          `json:"prHandoffSent,omitempty"`
	ResultArtifact string        `json:"resultArtifact,omitempty"`
	// The reviewer: its conversation (session and conversation id, which
	// outlives a resumed session), title, how people name the agent, and the
	// subscription it runs on.
	ReviewerSession      string `json:"reviewerSession"`
	ReviewerConversation string `json:"reviewerConversation,omitempty"`
	ReviewerTitle        string `json:"reviewerTitle,omitempty"`
	ReviewerLabel        string `json:"reviewerLabel"`
	ReviewerProvider     string `json:"reviewerProvider,omitempty"`
	ReviewerProfile      string `json:"reviewerProfile,omitempty"`
	// Fallback says why the reviewer is not the one asked for ("" when it is);
	// Focus is what the person asked it to look at in particular.
	Fallback string `json:"fallback,omitempty"`
	Focus    string `json:"focus,omitempty"`
	// The conversation whose work is reviewed.
	SourceSession      string `json:"sourceSession"`
	SourceConversation string `json:"sourceConversation,omitempty"`
	SourceTitle        string `json:"sourceTitle,omitempty"`
	SourceLabel        string `json:"sourceLabel,omitempty"`
	SourceProvider     string `json:"sourceProvider,omitempty"`
	// What is reviewed.
	Scope          string `json:"scope"`
	ScopeLabel     string `json:"scopeLabel"`
	Turn           int    `json:"turn,omitempty"`
	Compare        string `json:"compare,omitempty"`
	Member         string `json:"member,omitempty"`
	PullRequestURL string `json:"pullRequestUrl,omitempty"`
	// State and a sentence about it.
	State  string `json:"state"`
	Detail string `json:"detail,omitempty"`
	// The operation that delivered the brief.
	BriefOperation string `json:"briefOperation,omitempty"`
	// The mission task the source conversation is an attempt on.
	Mission   string `json:"mission,omitempty"`
	Task      string `json:"task,omitempty"`
	CreatedAt int64  `json:"createdAt"`
	UpdatedAt int64  `json:"updatedAt"`

	// The loop (loop.go): its mode and limit, the current round and phase,
	// whether the person paused it, why it ended, its rounds and timeline,
	// since when it waits for the author, and the agents' questions.
	Loop        string  `json:"loop,omitempty"`
	MaxRounds   int     `json:"maxRounds,omitempty"`
	Round       int     `json:"round,omitempty"`
	Phase       string  `json:"phase,omitempty"`
	Paused      bool    `json:"paused,omitempty"`
	StopReason  string  `json:"stopReason,omitempty"`
	Rounds      []Round `json:"rounds,omitempty"`
	Timeline    []Entry `json:"timeline,omitempty"`
	FixingSince int64   `json:"fixingSince,omitempty"`
	Asks        []Ask   `json:"asks,omitempty"`
	// Who ended the review by hand (cancelled it, or ended its reviewer),
	// and when: "You" for the person.
	StoppedBy string `json:"stoppedBy,omitempty"`
	StoppedAt int64  `json:"stoppedAt,omitempty"`
}

// Counts are a review's findings by severity that are still to act on
// (proposed, open or sent; not resolved, won't fix or dismissed).
type Counts struct{ Blockers, Majors, Minors, Nits int }

// Total is every finding counted.
func (c Counts) Total() int { return c.Blockers + c.Majors + c.Minors + c.Nits }

// Summary is the counts in words: "2 blockers, 3 minor" or "no open findings".
func (c Counts) Summary() string {
	var parts []string
	add := func(n int, one, many string) {
		if n == 1 {
			parts = append(parts, "1 "+one)
		} else if n > 1 {
			parts = append(parts, fmt.Sprintf("%d %s", n, many))
		}
	}
	add(c.Blockers, "blocker", "blockers")
	add(c.Majors, "major", "major")
	add(c.Minors, "minor", "minor")
	add(c.Nits, "nit", "nits")
	if len(parts) == 0 {
		return "no open findings"
	}
	return strings.Join(parts, ", ")
}

// Live reports whether a finding still asks for something.
func (c Comment) Live() bool {
	return c.Disposition != Resolved && c.Disposition != WontFix && c.Disposition != Dismissed
}

// CountFindings counts a run's live findings by severity.
func CountFindings(r Review, run string) Counts {
	var out Counts
	for _, c := range r.Comments {
		if c.ReviewID != run || !c.Live() {
			continue
		}
		switch c.Severity {
		case SeverityBlocker:
			out.Blockers++
		case SeverityMajor:
			out.Majors++
		case SeverityMinor:
			out.Minors++
		case SeverityNit:
			out.Nits++
		}
	}
	return out
}

// Findings are a run's findings, in the order they were filed.
func Findings(r Review, run string) []Comment {
	var out []Comment
	for _, c := range r.Comments {
		if c.ReviewID == run {
			out = append(out, c)
		}
	}
	return out
}

// Severities, most severe first (V1 review.rs, with nit kept apart).
const (
	SeverityBlocker = "blocker"
	SeverityMajor   = "major"
	SeverityMinor   = "minor"
	SeverityNit     = "nit"
)

// ErrSeverity refuses a severity word it cannot place. Unknown text is never
// silently downgraded: a typo must not demote a blocker (V1 review.rs).
var ErrSeverity = errors.New("severity must be blocker, major, minor or nit")

// ParseSeverity reads an agent-written severity word.
func ParseSeverity(text string) (string, error) {
	switch strings.ToLower(strings.TrimSpace(text)) {
	case "blocker", "blocking", "critical", "high", "p0":
		return SeverityBlocker, nil
	case "major", "medium", "important", "p1":
		return SeverityMajor, nil
	case "minor", "low", "p2":
		return SeverityMinor, nil
	case "nit", "nitpick", "trivial", "info", "style":
		return SeverityNit, nil
	}
	return "", fmt.Errorf("%w (got %q)", ErrSeverity, text)
}

// SeverityRank orders severities, most severe first; unknown last.
func SeverityRank(severity string) int {
	switch severity {
	case SeverityBlocker:
		return 0
	case SeverityMajor:
		return 1
	case SeverityMinor:
		return 2
	case SeverityNit:
		return 3
	}
	return 4
}

// FindingKey is a finding's key when the reviewer gave none: the same file,
// line and title is the same finding, so filing it again updates it.
func FindingKey(path string, line int, title string) string {
	sum := sha256.Sum256([]byte(fmt.Sprintf("%s\x00%d\x00%s", path, line, strings.ToLower(strings.Join(strings.Fields(title), " ")))))
	return "k-" + hex.EncodeToString(sum[:6])
}

const (
	titleLimit = 200
	keyLimit   = 128
	runsLimit  = 200
)

func newRunID() string { return strings.Replace(newID(), "rc-", "rv-", 1) }

func findRun(r *Review, id string) (*Run, error) {
	for i := range r.Runs {
		if r.Runs[i].ID == id {
			return &r.Runs[i], nil
		}
	}
	return nil, fmt.Errorf("review %q not found: %w", id, os.ErrNotExist)
}

// CreateRun records a new agent review in a workspace.
func (s *Store) CreateRun(workspace string, run Run) (Run, error) {
	if run.ReviewerSession == "" || run.SourceSession == "" {
		return Run{}, errors.New("a review needs its reviewer and source conversations")
	}
	now := s.Now().Unix()
	run.ID, run.CreatedAt, run.UpdatedAt = newRunID(), now, now
	if run.State == "" {
		run.State = RunRunning
	}
	_, err := s.update(workspace, func(r *Review) error {
		// The oldest finished reviews make room; their findings stay.
		for len(r.Runs) >= runsLimit {
			i := slices.IndexFunc(r.Runs, func(x Run) bool { return x.State != RunRunning && !x.Active() })
			if i < 0 {
				return fmt.Errorf("at most %d agent reviews per workspace", runsLimit)
			}
			r.Runs = slices.Delete(r.Runs, i, i+1)
		}
		r.Runs = append(r.Runs, run)
		return nil
	})
	if err != nil {
		return Run{}, err
	}
	s.index(workspace, run)
	return run, nil
}

// UpdateRun changes one review; change returning errUnchanged writes nothing.
func (s *Store) UpdateRun(workspace, id string, change func(*Run) error) (Run, error) {
	var out Run
	_, err := s.update(workspace, func(r *Review) error {
		run, err := findRun(r, id)
		if err != nil {
			return err
		}
		if err = change(run); err != nil {
			return err
		}
		run.UpdatedAt = s.Now().Unix()
		out = *run
		return nil
	})
	if err == nil {
		s.index(workspace, out)
	}
	return out, err
}

// Unchanged lets an UpdateRun change say it changed nothing.
var Unchanged = errUnchanged

// Roles a conversation can have in a review.
const (
	RoleReviewer = "reviewer"
	RoleAuthor   = "author"
)

// Match is a review a conversation takes part in, and its role there.
type Match struct {
	Workspace string
	Run       Run
	Role      string
}

// index remembers which workspace a run's conversations review in, so a
// turn's end or an agent's call finds its review without reading every file.
func (s *Store) index(workspace string, run Run) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.sessions == nil {
		return // built in full on first use
	}
	for _, id := range []string{run.ReviewerSession, run.ReviewerConversation, run.SourceSession, run.SourceConversation} {
		if id != "" && !slices.Contains(s.sessions[id], workspace) {
			s.sessions[id] = append(s.sessions[id], workspace)
		}
	}
}

// buildIndexLocked reads every workspace's review once.
func (s *Store) buildIndexLocked() {
	s.sessions = map[string][]string{}
	entries, err := os.ReadDir(s.dir)
	if err != nil {
		return
	}
	for _, entry := range entries {
		name := entry.Name()
		if entry.IsDir() || !strings.HasSuffix(name, ".json") {
			continue
		}
		workspace := strings.TrimSuffix(name, ".json")
		review, err := s.read(workspace)
		if err != nil {
			continue
		}
		for _, run := range review.Runs {
			for _, id := range []string{run.ReviewerSession, run.ReviewerConversation, run.SourceSession, run.SourceConversation} {
				if id != "" && !slices.Contains(s.sessions[id], workspace) {
					s.sessions[id] = append(s.sessions[id], workspace)
				}
			}
		}
	}
}

// RunsOf returns the reviews a conversation takes part in (by any of its
// ids: a session, or the conversation it continues), newest first.
func (s *Store) RunsOf(ids ...string) []Match {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.sessions == nil {
		s.buildIndexLocked()
	}
	want := map[string]bool{}
	workspaces := map[string]bool{}
	for _, id := range ids {
		if id == "" {
			continue
		}
		want[id] = true
		for _, w := range s.sessions[id] {
			workspaces[w] = true
		}
	}
	var out []Match
	for w := range workspaces {
		review, err := s.read(w)
		if err != nil {
			continue
		}
		for _, run := range review.Runs {
			switch {
			case want[run.ReviewerSession] || want[run.ReviewerConversation]:
				out = append(out, Match{Workspace: w, Run: run, Role: RoleReviewer})
			case want[run.SourceSession] || want[run.SourceConversation]:
				out = append(out, Match{Workspace: w, Run: run, Role: RoleAuthor})
			}
		}
	}
	slices.SortStableFunc(out, func(a, b Match) int { return int(b.Run.CreatedAt - a.Run.CreatedAt) })
	return out
}

// ReviewerRun is the review a conversation is the reviewer of.
func (s *Store) ReviewerRun(ids ...string) (Match, bool) {
	for _, m := range s.RunsOf(ids...) {
		if m.Role == RoleReviewer {
			return m, true
		}
	}
	return Match{}, false
}

// AddFinding files a reviewer's finding in its run: Author, Severity, Title,
// ReviewID and Key are set by the caller, the lines quoted from the file as
// it is. A finding with the run's key of one already filed updates its text,
// severity and lines (updated is true) and keeps its disposition: filing
// again is idempotent. A new finding is Proposed for the person to triage.
func (s *Store) AddFinding(workspace string, c Comment) (out Comment, updated bool, err error) {
	if c.ReviewID == "" || c.Author != AuthorAgent {
		return Comment{}, false, errors.New("a finding belongs to an agent review")
	}
	if c.Severity, err = ParseSeverity(c.Severity); err != nil {
		return Comment{}, false, err
	}
	c.Title = strings.Join(strings.Fields(c.Title), " ")
	if c.Title == "" || len(c.Title) > titleLimit {
		return Comment{}, false, fmt.Errorf("a finding needs a one-line title of at most %d bytes", titleLimit)
	}
	if strings.TrimSpace(c.Body) == "" {
		c.Body = c.Title
	}
	if c.Key == "" {
		c.Key = FindingKey(c.Path, c.StartLine, c.Title)
	}
	if len(c.Key) > keyLimit || strings.ContainsAny(c.Key, " \t\r\n\x00") {
		return Comment{}, false, fmt.Errorf("a finding key is one word of at most %d bytes", keyLimit)
	}
	c.Path = filepath.ToSlash(c.Path)
	// Validate everything Add checks, without storing.
	draft := c
	if draft.View == "" {
		draft.View = "uncommitted"
	}
	body, err := validBody(draft.Body)
	if err != nil {
		return Comment{}, false, err
	}
	if err = checkAnchor(draft); err != nil {
		return Comment{}, false, err
	}
	now := s.Now().Unix()
	_, err = s.update(workspace, func(r *Review) error {
		run, err := findRun(r, c.ReviewID)
		if err != nil {
			return err
		}
		if run.TemplateID != "" && run.State != RunRunning {
			return errors.New("the template review has already ended")
		}
		round := max(run.Round, 1)
		for i := range r.Comments {
			old := &r.Comments[i]
			if old.ReviewID == c.ReviewID && old.Key == c.Key {
				old.Body, old.Severity, old.Title, old.UpdatedAt = body, c.Severity, c.Title, now
				old.Path, old.Member, old.Side, old.StartLine, old.EndLine = c.Path, c.Member, SideNew, c.StartLine, c.StartLine+len(c.Snippet)-1
				old.Snippet, old.Hash, old.FileHash, old.Head = slices.Clone(c.Snippet), Hash(c.Snippet), c.FileHash, c.Head
				out, updated = *old, true
				return nil
			}
		}
		if len(r.Comments) >= commentsLimit {
			return fmt.Errorf("at most %d review comments per workspace; delete resolved ones first", commentsLimit)
		}
		disposition := c.Disposition
		if disposition == "" {
			disposition = Proposed
		}
		out = Comment{ID: newID(), Member: c.Member, Path: c.Path, Side: SideNew, StartLine: c.StartLine, EndLine: c.StartLine + len(c.Snippet) - 1,
			Snippet: slices.Clone(c.Snippet), Hash: Hash(c.Snippet), FileHash: c.FileHash, Body: body, Head: c.Head, View: draft.View, Base: c.Base,
			CreatedAt: now, UpdatedAt: now, Disposition: disposition,
			Author: AuthorAgent, AuthorLabel: c.AuthorLabel, AuthorSession: c.AuthorSession, Severity: c.Severity, Title: c.Title, ReviewID: c.ReviewID, Key: c.Key, Round: round}
		r.Comments = append(r.Comments, out)
		run.note(now, "filed", run.ReviewerLabel, fmt.Sprintf("%s: %s (%s/%s:%d)", severityWord(out.Severity), out.Title, out.Member, out.Path, out.StartLine), out.ID)
		return nil
	})
	if err != nil {
		return Comment{}, false, err
	}
	return out, updated, nil
}

// checkAnchor is Add's validation of where a comment points.
func checkAnchor(c Comment) error {
	if c.Member == "" || len(c.Member) > 256 || strings.ContainsAny(c.Member, "/\\\x00") {
		return errors.New("repository member required")
	}
	if !filepath.IsLocal(c.Path) || c.Path == "." || strings.ContainsRune(c.Path, 0) || len(c.Path) > 4096 {
		return errors.New("relative file path required")
	}
	if len(c.Snippet) == 0 || len(c.Snippet) > snippetLines {
		return fmt.Errorf("comment on 1 to %d lines", snippetLines)
	}
	size := 0
	for _, line := range c.Snippet {
		size += len(line) + 1
	}
	if size > snippetBytes {
		return errors.New("the quoted lines are limited to 64 KiB")
	}
	if c.StartLine < 1 {
		return errors.New("line numbers start at 1")
	}
	return nil
}
