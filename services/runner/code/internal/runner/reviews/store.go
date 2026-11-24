// Package reviews owns a person's inline review comments on a workspace's
// changes: anchored to a member, a file and a run of lines, with the commit
// and the diff they were made on and the lines they quote. Comments persist on
// the Runner per workspace, so every window shows the same ones and they
// survive restarts. Delivery to an agent is a message the person sends; this
// package only records that it was handed over.
package reviews

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/atomicfile"
)

// Sides of a diff a comment can be on: the file as it is ("new": added and
// unchanged lines) or the lines a change removed ("old").
const (
	SideNew = "new"
	SideOld = "old"
)

// Disposition is what became of a comment, after V1's finding outcomes.
type Disposition string

const (
	Open     Disposition = "open"
	Sent     Disposition = "sent"
	Resolved Disposition = "resolved"
	WontFix  Disposition = "wont_fix"
	// Proposed is a reviewer agent's finding the person has not triaged yet:
	// accepting it makes it Open (pending, to send); it is never sent as is.
	Proposed Disposition = "proposed"
	// Dismissed is a finding the person turned down, with their reason in Note.
	Dismissed Disposition = "dismissed"
)

func (d Disposition) valid() bool {
	return d == Open || d == Sent || d == Resolved || d == WontFix || d == Proposed || d == Dismissed
}

// Authors of a comment.
const (
	AuthorPerson = ""
	AuthorAgent  = "agent"
)

// Comment is one inline review comment.
type Comment struct {
	ID     string `json:"id"`
	Member string `json:"member"`
	Path   string `json:"path"`
	Side   string `json:"side"`
	// StartLine and EndLine are 1-based and inclusive, numbered as the side's
	// file was when the comment was made.
	StartLine int `json:"startLine"`
	EndLine   int `json:"endLine"`
	// Snippet is the lines as the person saw them; Hash is their content hash.
	Snippet []string `json:"snippet"`
	Hash    string   `json:"hash"`
	// FileHash is the working file's content hash when the comment was made.
	FileHash string `json:"fileHash,omitempty"`
	Body     string `json:"body"`
	// Head is the member's commit when the comment was made; View and Base
	// name the diff it was read in ("uncommitted", "staged" or "branch", and
	// what that diff compared with).
	Head      string `json:"head"`
	View      string `json:"view"`
	Base      string `json:"base,omitempty"`
	CreatedAt int64  `json:"createdAt"`
	UpdatedAt int64  `json:"updatedAt"`

	Disposition Disposition `json:"disposition"`
	// Where and when it was handed to an agent, and the commit it saw.
	SentTo      string `json:"sentTo,omitempty"`
	SentToTitle string `json:"sentToTitle,omitempty"`
	SentAt      int64  `json:"sentAt,omitempty"`
	SentHead    string `json:"sentHead,omitempty"`
	// ResolvedBy is the commit that changed the lines when the Runner resolved
	// the comment on its own; Note says why, for the person to read.
	ResolvedBy string `json:"resolvedBy,omitempty"`
	Note       string `json:"note,omitempty"`

	// A reviewer agent's finding: who filed it (Author is AuthorAgent; the
	// label is how people name the agent, the session its conversation), how
	// severe it is, its one-line title, the review it belongs to and its
	// stable key there. A person's comment leaves them empty.
	Author        string `json:"author,omitempty"`
	AuthorLabel   string `json:"authorLabel,omitempty"`
	AuthorSession string `json:"authorSession,omitempty"`
	Severity      string `json:"severity,omitempty"`
	Title         string `json:"title,omitempty"`
	ReviewID      string `json:"reviewId,omitempty"`
	Key           string `json:"key,omitempty"`
	// The loop (loop.go): the round a finding was filed in, whether it was
	// ever handed to the author, and how many later rounds found it still open.
	Round           int  `json:"round,omitempty"`
	Delivered       bool `json:"delivered,omitempty"`
	StillOpenRounds int  `json:"stillOpenRounds,omitempty"`
}

// Review is one workspace's comments, its agent reviews and its preference.
type Review struct {
	// AutoResolve resolves a sent comment when a later commit changes its lines.
	AutoResolve bool      `json:"autoResolve"`
	Comments    []Comment `json:"comments"`
	// Runs are the workspace's agent reviews, oldest first.
	Runs []Run `json:"runs,omitempty"`
}

// Handover records a set of comments put in a conversation's message box.
type Handover struct {
	Session, Title string
	// Heads is each member's commit at the handover, keyed by member.
	Heads map[string]string
}

const (
	bodyLimit     = 64 << 10
	snippetLines  = 400
	snippetBytes  = 64 << 10
	commentsLimit = 2000
)

var (
	workspaceID = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`)
	// ErrNotFound is a comment that is not (or no longer) in the workspace.
	errUnchanged = errors.New("unchanged")
	ErrNotFound  = fmt.Errorf("review comment not found; it may have been deleted in another window: %w", os.ErrNotExist)
)

// Store keeps each workspace's review in its own private file.
type Store struct {
	mu  sync.Mutex
	dir string
	// sessions maps a conversation's ids to the workspaces whose reviews it
	// takes part in (runs.go); nil until first used.
	sessions map[string][]string
	// Now is the clock; tests replace it.
	Now func() time.Time
}

// New prepares the directory that holds every workspace's review.
func New(dir string) (*Store, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	return &Store{dir: dir, Now: time.Now}, nil
}

func (s *Store) file(workspace string) (string, error) {
	if !workspaceID.MatchString(workspace) {
		return "", errors.New("invalid workspace id")
	}
	return filepath.Join(s.dir, workspace+".json"), nil
}

func (s *Store) read(workspace string) (Review, error) {
	path, err := s.file(workspace)
	if err != nil {
		return Review{}, err
	}
	raw, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return Review{}, nil
	}
	if err != nil {
		return Review{}, err
	}
	var review Review
	if err = json.Unmarshal(raw, &review); err != nil {
		return Review{}, fmt.Errorf("unreadable review comments for this workspace: %w", err)
	}
	return review, nil
}

func (s *Store) write(workspace string, review Review) error {
	path, err := s.file(workspace)
	if err != nil {
		return err
	}
	raw, err := json.Marshal(review)
	if err != nil {
		return err
	}
	return atomicfile.WriteFile(path, raw)
}

// update applies change to a workspace's review and persists it; nothing
// changes in memory or on disk when change or the write fails.
func (s *Store) update(workspace string, change func(*Review) error) (Review, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	review, err := s.read(workspace)
	if err != nil {
		return Review{}, err
	}
	if err = change(&review); errors.Is(err, errUnchanged) {
		return review, nil
	} else if err != nil {
		return Review{}, err
	}
	if err = s.write(workspace, review); err != nil {
		return Review{}, err
	}
	return review, nil
}

// Load returns a workspace's review; a workspace without one has none.
func (s *Store) Load(workspace string) (Review, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.read(workspace)
}

func validBody(body string) (string, error) {
	body = strings.TrimRight(body, " \t\r\n")
	if strings.TrimSpace(body) == "" {
		return "", errors.New("write a comment first")
	}
	if len(body) > bodyLimit {
		return "", errors.New("a comment is limited to 64 KiB")
	}
	return body, nil
}

func newID() string {
	var b [6]byte
	_, _ = rand.Read(b[:])
	return "rc-" + hex.EncodeToString(b[:])
}

// Add records a new open comment and returns it with its id.
func (s *Store) Add(workspace string, c Comment) (Comment, error) {
	body, err := validBody(c.Body)
	if err != nil {
		return Comment{}, err
	}
	if c.Member == "" || len(c.Member) > 256 || strings.ContainsAny(c.Member, "/\\\x00") {
		return Comment{}, errors.New("repository member required")
	}
	if !filepath.IsLocal(c.Path) || c.Path == "." || strings.ContainsRune(c.Path, 0) || len(c.Path) > 4096 {
		return Comment{}, errors.New("relative file path required")
	}
	if c.Side != SideNew && c.Side != SideOld {
		return Comment{}, errors.New(`side must be "new" or "old"`)
	}
	if len(c.Snippet) == 0 || len(c.Snippet) > snippetLines {
		return Comment{}, fmt.Errorf("comment on 1 to %d lines", snippetLines)
	}
	size := 0
	for _, line := range c.Snippet {
		size += len(line) + 1
	}
	if size > snippetBytes {
		return Comment{}, errors.New("the quoted lines are limited to 64 KiB")
	}
	if c.StartLine < 1 {
		return Comment{}, errors.New("line numbers start at 1")
	}
	if c.View != "uncommitted" && c.View != "staged" && c.View != "branch" {
		return Comment{}, errors.New(`view must be "uncommitted", "staged" or "branch"`)
	}
	now := s.Now().Unix()
	c = Comment{ID: newID(), Member: c.Member, Path: filepath.ToSlash(c.Path), Side: c.Side, StartLine: c.StartLine, EndLine: c.StartLine + len(c.Snippet) - 1,
		Snippet: slices.Clone(c.Snippet), Hash: Hash(c.Snippet), FileHash: c.FileHash, Body: body, Head: c.Head, View: c.View, Base: c.Base,
		CreatedAt: now, UpdatedAt: now, Disposition: Open}
	_, err = s.update(workspace, func(r *Review) error {
		if len(r.Comments) >= commentsLimit {
			return fmt.Errorf("at most %d review comments per workspace; delete resolved ones first", commentsLimit)
		}
		r.Comments = append(r.Comments, c)
		return nil
	})
	return c, err
}

func find(r *Review, id string) (*Comment, error) {
	for i := range r.Comments {
		if r.Comments[i].ID == id {
			return &r.Comments[i], nil
		}
	}
	return nil, ErrNotFound
}

// Edit replaces a comment's text.
func (s *Store) Edit(workspace, id, body string) (Review, error) {
	body, err := validBody(body)
	if err != nil {
		return Review{}, err
	}
	return s.update(workspace, func(r *Review) error {
		c, err := find(r, id)
		if err != nil {
			return err
		}
		c.Body, c.UpdatedAt = body, s.Now().Unix()
		return nil
	})
}

// Update applies change to a workspace's review and persists it, for the
// agent review records (runs.go).
func (s *Store) Update(workspace string, change func(*Review) error) (Review, error) {
	return s.update(workspace, change)
}

// Delete removes a comment.
func (s *Store) Delete(workspace, id string) (Review, error) {
	return s.update(workspace, func(r *Review) error {
		if _, err := find(r, id); err != nil {
			return err
		}
		r.Comments = slices.DeleteFunc(r.Comments, func(c Comment) bool { return c.ID == id })
		return nil
	})
}

// SetDisposition moves comments to open, sent, resolved, won't fix or
// dismissed. Sent records the handover; open clears it, so a comment taken
// back is pending again (accepting a proposed finding is moving it to open).
// Dismissed keeps the reason as the note. Unknown ids fail the whole change.
func (s *Store) SetDisposition(workspace string, ids []string, d Disposition, to Handover, note ...string) (Review, error) {
	if !d.valid() || d == Proposed {
		return Review{}, errors.New("disposition must be open, sent, resolved, wont_fix or dismissed")
	}
	reason := ""
	if len(note) > 0 {
		reason = strings.TrimSpace(note[0])
	}
	if len(reason) > 2000 {
		return Review{}, errors.New("a reason is limited to 2000 bytes")
	}
	if len(ids) == 0 {
		return Review{}, errors.New("no review comments chosen")
	}
	if d == Sent && to.Session == "" {
		return Review{}, errors.New("choose the conversation the comments go to")
	}
	return s.update(workspace, func(r *Review) error {
		now := s.Now().Unix()
		for _, id := range ids {
			c, err := find(r, id)
			if err != nil {
				return err
			}
			c.Disposition, c.UpdatedAt, c.ResolvedBy, c.Note = d, now, "", ""
			if d == Dismissed {
				c.Note = reason
			}
			switch d {
			case Sent:
				c.SentTo, c.SentToTitle, c.SentHead, c.SentAt = to.Session, to.Title, to.Heads[c.Member], now
			case Open:
				c.SentTo, c.SentToTitle, c.SentHead, c.SentAt = "", "", "", 0
			}
		}
		return nil
	})
}

// SetAutoResolve turns automatic resolution on or off for a workspace.
func (s *Store) SetAutoResolve(workspace string, on bool) (Review, error) {
	return s.update(workspace, func(r *Review) error { r.AutoResolve = on; return nil })
}

// ResolveByCommit resolves a comment that is still sent because commit
// changed its lines. A comment the person reopened or settled meanwhile is
// left alone; the result says whether it changed.
func (s *Store) ResolveByCommit(workspace, id, commit string) (bool, error) {
	changed := false
	_, err := s.update(workspace, func(r *Review) error {
		c, err := find(r, id)
		if err != nil || c.Disposition != Sent || !r.AutoResolve {
			return errUnchanged
		}
		short := commit
		if len(short) > 8 {
			short = short[:8]
		}
		c.Disposition, c.ResolvedBy, c.UpdatedAt, changed = Resolved, commit, s.Now().Unix(), true
		c.Note = "Resolved automatically: commit " + short + " changed these lines."
		return nil
	})
	return changed, err
}

// Forget removes a workspace's review, when the workspace is deleted.
func (s *Store) Forget(workspace string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	path, err := s.file(workspace)
	if err != nil {
		return err
	}
	if err = os.Remove(path); errors.Is(err, os.ErrNotExist) {
		return nil
	}
	return err
}

// AuthorGitHub marks a comment imported from a GitHub pull request review;
// its AuthorLabel names the reviewer ("@octocat on GitHub").
const AuthorGitHub = "github"

// Import adds comments from a code host's review (a GitHub pull request's
// inline comments), each once by its Key: one already in the workspace, in
// whatever state, is left as it is. They are pending, as the person's own
// comments are, and anchor the same way. added is what was new.
func (s *Store) Import(workspace string, comments []Comment) (added []Comment, err error) {
	now := s.Now().Unix()
	_, err = s.update(workspace, func(r *Review) error {
		have := map[string]bool{}
		for _, c := range r.Comments {
			if c.Key != "" {
				have[c.Key] = true
			}
		}
		for _, c := range comments {
			if c.Key == "" || have[c.Key] {
				continue
			}
			body, err := validBody(c.Body)
			if err != nil {
				continue
			}
			if c.Side != SideOld {
				c.Side = SideNew
			}
			if c.View == "" {
				c.View = "branch"
			}
			if checkAnchor(c) != nil {
				continue
			}
			if len(r.Comments) >= commentsLimit {
				return fmt.Errorf("at most %d review comments per workspace; delete resolved ones first", commentsLimit)
			}
			c = Comment{ID: newID(), Member: c.Member, Path: filepath.ToSlash(c.Path), Side: c.Side, StartLine: c.StartLine, EndLine: c.StartLine + len(c.Snippet) - 1,
				Snippet: slices.Clone(c.Snippet), Hash: Hash(c.Snippet), FileHash: c.FileHash, Body: body, Head: c.Head, View: c.View, Base: c.Base,
				CreatedAt: now, UpdatedAt: now, Disposition: Open, Author: c.Author, AuthorLabel: c.AuthorLabel, Key: c.Key}
			r.Comments = append(r.Comments, c)
			have[c.Key] = true
			added = append(added, c)
		}
		if len(added) == 0 {
			return errUnchanged
		}
		return nil
	})
	return added, err
}
