// Package artifacts keeps the versioned documents of each workspace: plans,
// reports, specs and notes taken from a conversation message, a repository
// file, or published by an agent. Versions are append-only; nothing here
// rewrites or drops one. See ARTIFACTS.md.
package artifacts

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/atomicfile"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
)

// Limits on what one artifact holds.
const (
	MaxContent  = 1 << 20
	MaxVersions = 1000
	MaxTitle    = 200
)

// Kinds an artifact may have; the first is the default.
var Kinds = []string{"other", "plan", "report", "spec", "notes", "todo-list"}

var (
	// ErrInvalid wraps a request the Runner will never accept as written.
	ErrInvalid = errors.New("invalid artifact")
	// ErrConflict refuses a write made against a version that is no longer
	// the latest: the writer reviews the newer version first.
	ErrConflict = errors.New("artifact changed")
)

type Author struct {
	// person, agent or file.
	Kind      string `json:"kind"`
	SessionID string `json:"sessionId,omitempty"`
}

type Source struct {
	// message, file, agent or edit.
	Kind           string `json:"kind"`
	SessionID      string `json:"sessionId,omitempty"`
	ConversationID string `json:"conversationId,omitempty"`
	MessageID      string `json:"messageId,omitempty"`
	TurnID         string `json:"turnId,omitempty"`
	Path           string `json:"path,omitempty"`
}

// Link pins an input revision for a derived roadmap.
type Link struct {
	Version uint32 `json:"version,omitempty"`
	Kind    string `json:"kind"`
	ID      string `json:"id"`
	Role    string `json:"role"`
}

type Version struct {
	Schema    string `json:"schema,omitempty"`
	Number    uint32 `json:"number"`
	CreatedAt int64  `json:"createdAt"`
	Author    Author `json:"author"`
	Source    Source `json:"source"`
	Digest    string `json:"digest"`
	Size      uint64 `json:"size"`
}

type Artifact struct {
	CreationDigest       string    `json:"creationDigest,omitempty"`
	ID                   string    `json:"id"`
	Workspace            string    `json:"workspace"`
	Title                string    `json:"title"`
	Kind                 string    `json:"kind"`
	Format               string    `json:"format"`
	CreatedAt            int64     `json:"createdAt"`
	UpdatedAt            int64     `json:"updatedAt"`
	Versions             []Version `json:"versions"`
	OriginSessionID      string    `json:"originSessionId,omitempty"`
	OriginConversationID string    `json:"originConversationId,omitempty"`
	TrackedPath          string    `json:"trackedPath,omitempty"`
	TrackingProblem      string    `json:"trackingProblem,omitempty"`
	Links                []Link    `json:"links,omitempty"`
	// Retired is set while the artifact is retired: kept with every version,
	// left out of the default list, restorable.
	Retired *Retirement `json:"retired,omitempty"`
}

// Retirement records who retired an artifact, when, and what replaces it.
type Retirement struct {
	At int64  `json:"at"`
	By Author `json:"by"`
	// SupersededBy is the artifact of the same workspace that replaces it.
	SupersededBy string `json:"supersededBy,omitempty"`
	Reason       string `json:"reason,omitempty"`
}

// MaxReason bounds the one line a retirement may carry.
const MaxReason = 500

// AgentPublished reports whether an agent wrote every version and no file is
// followed: the only artifacts an agent may retire. One the person tracked,
// edited or took from a message is theirs.
func (a Artifact) AgentPublished() bool {
	if a.TrackedPath != "" || len(a.Versions) == 0 {
		return false
	}
	for _, v := range a.Versions {
		if v.Author.Kind != "agent" {
			return false
		}
	}
	return true
}

// Latest is the newest version's number.
func (a Artifact) Latest() uint32 {
	if len(a.Versions) == 0 {
		return 0
	}
	return a.Versions[len(a.Versions)-1].Number
}

// Document is an artifact with one version's content.
type Document struct {
	Artifact
	Content        string
	ContentVersion uint32
}

// Put creates an artifact (ID empty) or appends a version to one.
type Put struct {
	CreationKey                      string
	ID, Title, Kind, Format, Content string
	Author                           Author
	Source                           Source
	// ExpectedVersion, when not 0, must be the latest version.
	ExpectedVersion uint32
	Links           []Link
}

type Store struct {
	mu    sync.Mutex
	root  string
	items map[string]map[string]*Artifact
	now   func() time.Time
	// pending is the watcher's last sighting of changed file content, per artifact.
	pending map[string]sighting
}

type sighting struct {
	digest string
	since  time.Time
}

var segment = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$`)

// Open loads every artifact under root, creating it if needed.
func Open(root string) (*Store, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("artifact store needs an absolute directory")
	}
	if err := os.MkdirAll(root, 0o700); err != nil {
		return nil, err
	}
	s := &Store{root: root, items: map[string]map[string]*Artifact{}, now: time.Now, pending: map[string]sighting{}}
	workspaces, err := os.ReadDir(root)
	if err != nil {
		return nil, err
	}
	for _, w := range workspaces {
		if !w.IsDir() || !segment.MatchString(w.Name()) {
			continue
		}
		entries, err := os.ReadDir(filepath.Join(root, w.Name()))
		if err != nil {
			return nil, err
		}
		for _, e := range entries {
			if !e.IsDir() || !segment.MatchString(e.Name()) {
				continue
			}
			raw, err := os.ReadFile(filepath.Join(root, w.Name(), e.Name(), "artifact.json"))
			if errors.Is(err, fs.ErrNotExist) {
				continue // a create interrupted before its metadata: nothing was promised
			}
			if err != nil {
				return nil, err
			}
			a := &Artifact{}
			if err = json.Unmarshal(raw, a); err != nil {
				return nil, fmt.Errorf("artifact %s/%s is unreadable: %w", w.Name(), e.Name(), err)
			}
			if a.ID != e.Name() || a.Workspace != w.Name() || len(a.Versions) == 0 {
				return nil, fmt.Errorf("artifact %s/%s is inconsistent", w.Name(), e.Name())
			}
			s.workspace(w.Name())[a.ID] = a
		}
	}
	return s, nil
}

func (s *Store) workspace(ws string) map[string]*Artifact {
	items := s.items[ws]
	if items == nil {
		items = map[string]*Artifact{}
		s.items[ws] = items
	}
	return items
}

func validWorkspace(ws string) error {
	if !segment.MatchString(ws) {
		return fmt.Errorf("%w: a workspace is required", ErrInvalid)
	}
	return nil
}

func (s *Store) find(ws, id string) (*Artifact, error) {
	if err := validWorkspace(ws); err != nil {
		return nil, err
	}
	if a := s.items[ws][id]; a != nil && segment.MatchString(id) {
		return a, nil
	}
	return nil, fmt.Errorf("artifact %q: %w", id, fs.ErrNotExist)
}

// HasWorkspace reports whether a deleted workspace still has documents to list.
func (s *Store) HasWorkspace(ws string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.items[ws]) > 0
}

// List returns a workspace's artifacts, most recently updated first.
func (s *Store) List(ws string) ([]Artifact, error) {
	if err := validWorkspace(ws); err != nil {
		return nil, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]Artifact, 0, len(s.items[ws]))
	for _, a := range s.items[ws] {
		out = append(out, clone(a))
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].UpdatedAt != out[j].UpdatedAt {
			return out[i].UpdatedAt > out[j].UpdatedAt
		}
		return out[i].ID < out[j].ID
	})
	return out, nil
}

// Get returns one version (0 is the latest) with its content.
func (s *Store) Get(ws, id string, version uint32) (Document, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	a, err := s.find(ws, id)
	if err != nil {
		return Document{}, err
	}
	return s.document(a, version)
}

func (s *Store) document(a *Artifact, version uint32) (Document, error) {
	if version == 0 {
		version = a.Latest()
	}
	if version > a.Latest() {
		return Document{}, fmt.Errorf("version %d of %q: %w", version, a.ID, fs.ErrNotExist)
	}
	raw, err := os.ReadFile(s.versionPath(a, version))
	if err != nil {
		return Document{}, err
	}
	if version == 0 || uint64(version) > uint64(len(a.Versions)) || a.Versions[version-1].Number != version {
		return Document{}, fmt.Errorf("artifact %q has inconsistent metadata for version %d", a.ID, version)
	}
	content := string(raw)
	if Digest(content) != a.Versions[version-1].Digest {
		return Document{}, fmt.Errorf("artifact %q version %d failed its content integrity check", a.ID, version)
	}
	return Document{Artifact: clone(a), Content: content, ContentVersion: version}, nil
}

func (s *Store) dir(ws, id string) string { return filepath.Join(s.root, ws, id) }

var formatExtensions = map[string]string{"markdown": ".md", "html": ".html", "json": ".json"}

func (s *Store) versionPath(a *Artifact, n uint32) string {
	extension := formatExtensions[a.Format]
	if extension == "" {
		extension = formatExtensions["markdown"]
	}
	return filepath.Join(s.dir(a.Workspace, a.ID), "v"+strconv.FormatUint(uint64(n), 10)+extension)
}

func normalizeTitle(title string) (string, error) {
	title = strings.TrimSpace(title)
	if title == "" || utf8.RuneCountInString(title) > MaxTitle || !utf8.ValidString(title) || strings.IndexFunc(title, unicode.IsControl) >= 0 {
		return "", fmt.Errorf("%w: a title of 1 to %d characters, on one line, is required", ErrInvalid, MaxTitle)
	}
	return title, nil
}

func validKind(kind string) error {
	for _, k := range Kinds {
		if k == kind {
			return nil
		}
	}
	return fmt.Errorf("%w: kind must be plan, report, spec, notes, todo-list or other", ErrInvalid)
}

func validContent(content string) error {
	if strings.TrimSpace(content) == "" {
		return fmt.Errorf("%w: an artifact needs content", ErrInvalid)
	}
	if len(content) > MaxContent || !utf8.ValidString(content) || strings.ContainsRune(content, 0) {
		return fmt.Errorf("%w: content must be UTF-8 text of at most 1 MiB", ErrInvalid)
	}
	return nil
}

func (s *Store) validateInputs(ws string, p Put) error {
	roles := map[string]bool{}
	ids := map[string]bool{}
	for _, link := range p.Links {
		if link.Kind != "artifact" {
			return errors.New("mission and task links are not supported")
		}
		if p.ID != "" || p.Kind != "plan" || (link.Role != "design" && link.Role != "review") || roles[link.Role] || ids[link.ID] {
			return fmt.Errorf("%w: roadmap inputs require a new plan and distinct sources and roles", ErrInvalid)
		}
		source, err := s.find(ws, link.ID)
		if err != nil {
			return err
		}
		if link.Version == 0 || link.Version > source.Latest() {
			return fmt.Errorf("%w: unknown source revision", ErrInvalid)
		}
		roles[link.Role], ids[link.ID] = true, true
	}
	return nil
}

// Digest is the hex sha256 the Runner records for content.
func Digest(content string) string {
	sum := sha256.Sum256([]byte(content))
	return hex.EncodeToString(sum[:])
}

func newID() string { return strings.ToLower(rand.Text()) }

// Put creates or appends. Content identical to the latest version, with no
// change of title or kind, writes nothing and returns the artifact as it is,
// so a retried put is harmless.
func (s *Store) Put(ws string, p Put) (Document, error) {
	if err := validWorkspace(ws); err != nil {
		return Document{}, err
	}
	if err := validContent(p.Content); err != nil {
		return Document{}, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	id := ""
	creationDigest := ""
	if p.CreationKey != "" {
		if p.ID != "" || !segment.MatchString(p.CreationKey) {
			return Document{}, fmt.Errorf("%w: invalid artifact creation key", ErrInvalid)
		}
		id = "create-" + Digest(p.Author.Kind+"\x00"+p.Author.SessionID+"\x00"+p.CreationKey)
		encoded, err := json.Marshal(p)
		if err != nil {
			return Document{}, err
		}
		creationDigest = Digest(string(encoded))
		if existing := s.items[ws][id]; existing != nil {
			if existing.CreationDigest != creationDigest {
				return Document{}, fmt.Errorf("%w: creation key was already used with different inputs", ErrConflict)
			}
			return s.document(existing, 1)
		}
	}
	if err := s.validateInputs(ws, p); err != nil {
		return Document{}, err
	}
	if p.ID == "" {
		if id == "" {
			id = newID()
		}
		return s.createWithID(ws, id, p.Title, p.Kind, p.Content, p.Author, p.Source, "", "", p.Format, creationDigest, p.Links...)
	}
	a, err := s.find(ws, p.ID)
	if err != nil {
		return Document{}, err
	}
	if p.Format != "" && p.Format != a.Format {
		return Document{}, fmt.Errorf("%w: an artifact format cannot change", ErrInvalid)
	}
	if p.ExpectedVersion != 0 && p.ExpectedVersion != a.Latest() {
		return Document{}, fmt.Errorf("%w: a newer version (v%d) exists; review it before saving", ErrConflict, a.Latest())
	}
	next := clone(a)
	if p.Title != "" {
		if next.Title, err = normalizeTitle(p.Title); err != nil {
			return Document{}, err
		}
	}
	if p.Kind != "" {
		if a.Format == "json" && p.Kind != a.Kind || (p.Kind == "todo-list") != (a.Kind == "todo-list") {
			return Document{}, fmt.Errorf("%w: cannot change structured artifact kind", ErrInvalid)
		}
		if err = validKind(p.Kind); err != nil {
			return Document{}, err
		}
		next.Kind = p.Kind
	}
	changed := Digest(p.Content) != a.Versions[len(a.Versions)-1].Digest
	if !changed && next.Title == a.Title && next.Kind == a.Kind {
		return s.document(a, 0)
	}
	if changed {
		if err = s.append(&next, p.Content, p.Author, p.Source, ""); err != nil {
			return Document{}, err
		}
	} else {
		next.UpdatedAt = s.now().UnixMilli()
		if err = s.save(&next); err != nil {
			return Document{}, err
		}
	}
	*a = next
	return s.document(a, 0)
}

func (s *Store) create(ws, title, kind, content string, author Author, source Source, tracked string) (Document, error) {
	return s.createWithID(ws, newID(), title, kind, content, author, source, tracked, "", "markdown", "")
}

// PublishReview retains each round under the review run ID. Source.TurnID
// identifies a round for idempotent retries across Runner restarts. An empty
// TurnID preserves the original single-result behavior.
func (s *Store) PublishReview(ws, id, title, content, schema string, source Source) (Document, error) {
	if err := validWorkspace(ws); err != nil {
		return Document{}, err
	}
	if !segment.MatchString(id) {
		return Document{}, errors.New("invalid review artifact id")
	}
	if err := validContent(content); err != nil {
		return Document{}, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if existing := s.items[ws][id]; existing != nil {
		if source.TurnID == "" {
			return s.document(existing, 1)
		}
		for _, version := range existing.Versions {
			if version.Source.TurnID == source.TurnID {
				return s.document(existing, version.Number)
			}
		}
		if err := s.append(existing, content, Author{Kind: "agent", SessionID: source.SessionID}, source, schema); err != nil {
			return Document{}, err
		}
		return s.document(existing, existing.Latest())
	}
	return s.createWithID(ws, id, title, "report", content, Author{Kind: "agent", SessionID: source.SessionID}, source, "", schema, "markdown", "")
}

func (s *Store) createWithID(ws, id, title, kind, content string, author Author, source Source, tracked, schema, format, creationDigest string, links ...Link) (Document, error) {
	title, err := normalizeTitle(title)
	if err != nil {
		return Document{}, err
	}
	// Recognize a template on the normal publish path, including stdin.
	if format == "" && strings.HasPrefix(strings.TrimSpace(content), "{") {
		var header struct{ Schema string }
		if json.Unmarshal([]byte(content), &header) == nil {
			for _, candidate := range Kinds {
				if header.Schema == candidate+".v1" || candidate == "spec" && header.Schema == "spec.v2" || candidate == "report" && header.Schema == delivery.OutcomeSchema {
					format = "json"
					if kind == "" {
						kind = candidate
					}
					break
				}
			}
		}
	}
	if kind == "" {
		kind = Kinds[0]
	}
	if err = validKind(kind); err != nil {
		return Document{}, err
	}
	if kind == "todo-list" && format == "" {
		format = "json"
	}
	if kind == "todo-list" && format != "json" {
		return Document{}, fmt.Errorf("%w: todo-list requires JSON", ErrInvalid)
	}
	if format == "" {
		format = "markdown"
	}
	if _, ok := formatExtensions[format]; !ok {
		return Document{}, fmt.Errorf("%w: unknown artifact format %q", ErrInvalid, format)
	}
	now := s.now().UnixMilli()
	a := Artifact{CreationDigest: creationDigest, Links: append([]Link(nil), links...), ID: id, Workspace: ws, Title: title, Kind: kind, Format: format, CreatedAt: now, TrackedPath: tracked}
	if source.Kind == "message" || source.Kind == "agent" {
		a.OriginSessionID, a.OriginConversationID = source.SessionID, source.ConversationID
	}
	if err = os.MkdirAll(s.dir(ws, a.ID), 0o700); err != nil {
		return Document{}, err
	}
	if err = s.append(&a, content, author, source, schema); err != nil {
		_ = os.RemoveAll(s.dir(ws, a.ID))
		return Document{}, err
	}
	stored := a
	s.workspace(ws)[a.ID] = &stored
	return s.document(&stored, 0)
}

// append writes the next version's body, then the metadata naming it.
func (s *Store) append(a *Artifact, content string, author Author, source Source, schema string) error {
	if a.Format == "json" {
		if err := validateStructured(a.Kind, content); err != nil {
			return err
		}
		schema = structuredSchema(a.Kind, content)
	}
	if len(a.Versions) >= MaxVersions {
		return fmt.Errorf("%q has reached %d versions; track a new artifact to keep going", a.Title, MaxVersions)
	}
	now := s.now().UnixMilli()
	v := Version{Schema: schema, Number: a.Latest() + 1, CreatedAt: now, Author: author, Source: source, Digest: Digest(content), Size: uint64(len(content))}
	if err := atomicfile.WriteFile(s.versionPath(a, v.Number), []byte(content)); err != nil {
		return err
	}
	a.Versions = append(a.Versions, v)
	a.UpdatedAt = now
	if err := s.save(a); err != nil {
		a.Versions = a.Versions[:len(a.Versions)-1]
		return err
	}
	return nil
}

func (s *Store) save(a *Artifact) error {
	raw, err := json.Marshal(a)
	if err != nil {
		return err
	}
	return atomicfile.WriteFile(filepath.Join(s.dir(a.Workspace, a.ID), "artifact.json"), raw)
}

// FirstHeading is the text of the first level-one ATX heading, or "".
func FirstHeading(content string) string {
	inFence := false
	for _, line := range strings.Split(content, "\n") {
		trimmed := strings.TrimSpace(line)
		if strings.HasPrefix(trimmed, "```") || strings.HasPrefix(trimmed, "~~~") {
			inFence = !inFence
			continue
		}
		if inFence {
			continue
		}
		if title, ok := strings.CutPrefix(trimmed, "# "); ok {
			title = strings.TrimSpace(strings.TrimRight(strings.TrimSpace(title), "#"))
			if title != "" {
				return title
			}
		}
	}
	return ""
}

// Track creates an artifact from a workspace file the Runner then follows.
// Tracking a path already tracked returns that artifact unchanged.
func (s *Store) Track(ws, file, title, kind, content string, author Author) (Document, error) {
	if err := validWorkspace(ws); err != nil {
		return Document{}, err
	}
	if file == "" {
		return Document{}, fmt.Errorf("%w: a workspace file is required", ErrInvalid)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, a := range s.items[ws] {
		if a.TrackedPath == file {
			return s.document(a, 0)
		}
	}
	if err := validContent(content); err != nil {
		return Document{}, err
	}
	if strings.TrimSpace(title) == "" {
		title = FirstHeading(content)
	}
	if strings.TrimSpace(title) == "" {
		base := path.Base(file)
		title = strings.TrimSuffix(base, path.Ext(base))
	}
	return s.create(ws, title, kind, content, author, Source{Kind: "file", Path: file}, file)
}

// Update renames, changes kind, or stops following the tracked file.
func (s *Store) Update(ws, id, title, kind string, stopTracking bool) (Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	a, err := s.find(ws, id)
	if err != nil {
		return Artifact{}, err
	}
	next := clone(a)
	if title != "" {
		if next.Title, err = normalizeTitle(title); err != nil {
			return Artifact{}, err
		}
	}
	if kind != "" {
		if a.Format == "json" && kind != a.Kind || (kind == "todo-list") != (a.Kind == "todo-list") {
			return Artifact{}, fmt.Errorf("%w: cannot change structured artifact kind", ErrInvalid)
		}
		if err = validKind(kind); err != nil {
			return Artifact{}, err
		}
		next.Kind = kind
	}
	if stopTracking {
		next.TrackedPath, next.TrackingProblem = "", ""
		delete(s.pending, ws+"/"+id)
	}
	if err = s.save(&next); err != nil {
		return Artifact{}, err
	}
	*a = next
	return clone(a), nil
}

// SetTracked starts following a workspace file (after saving the artifact there).
func (s *Store) SetTracked(ws, id, file string) (Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	a, err := s.find(ws, id)
	if err != nil {
		return Artifact{}, err
	}
	if file == "" {
		return Artifact{}, fmt.Errorf("%w: a workspace file is required", ErrInvalid)
	}
	for _, other := range s.items[ws] {
		if other.ID != id && other.TrackedPath == file {
			return Artifact{}, fmt.Errorf("%s is already tracked as %q", file, other.Title)
		}
	}
	next := clone(a)
	next.TrackedPath, next.TrackingProblem = file, ""
	if err = s.save(&next); err != nil {
		return Artifact{}, err
	}
	*a = next
	delete(s.pending, ws+"/"+id)
	return clone(a), nil
}

// Delete removes an artifact and every version.
func (s *Store) Delete(ws, id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, err := s.find(ws, id); err != nil {
		return err
	}
	if err := os.RemoveAll(s.dir(ws, id)); err != nil {
		return err
	}
	delete(s.items[ws], id)
	delete(s.pending, ws+"/"+id)
	return nil
}

// FinishWorkspaceDeletion stops following removed files when their saved versions are kept.
func (s *Store) FinishWorkspaceDeletion(ws string, keep bool) error {
	if !keep {
		return s.DeleteWorkspace(ws)
	}
	items, err := s.List(ws)
	if err != nil {
		return err
	}
	for _, a := range items {
		if a.TrackedPath != "" {
			if _, err = s.Update(ws, a.ID, "", "", true); err != nil && !errors.Is(err, fs.ErrNotExist) {
				return err
			}
		}
	}
	return nil
}

// DeleteWorkspace removes every artifact of a deleted workspace.
func (s *Store) DeleteWorkspace(ws string) error {
	if validWorkspace(ws) != nil {
		return nil // no artifact can be stored under such a name
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := os.RemoveAll(filepath.Join(s.root, ws)); err != nil {
		return err
	}
	for id := range s.items[ws] {
		delete(s.pending, ws+"/"+id)
	}
	delete(s.items, ws)
	return nil
}

// Retire archives an artifact: it keeps every version and leaves the default
// list. supersededBy, when not empty, must be another artifact of the same
// workspace. Retiring an artifact already retired records the new retirement.
func (s *Store) Retire(ws, id string, by Author, supersededBy, reason string) (Artifact, error) {
	reason = strings.TrimSpace(reason)
	if utf8.RuneCountInString(reason) > MaxReason || !utf8.ValidString(reason) || strings.IndexFunc(reason, unicode.IsControl) >= 0 {
		return Artifact{}, fmt.Errorf("%w: a reason is one line of at most %d characters", ErrInvalid, MaxReason)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	a, err := s.find(ws, id)
	if err != nil {
		return Artifact{}, err
	}
	if supersededBy != "" {
		if supersededBy == id {
			return Artifact{}, fmt.Errorf("%w: an artifact cannot supersede itself", ErrInvalid)
		}
		if _, err = s.find(ws, supersededBy); err != nil {
			return Artifact{}, fmt.Errorf("%w: the superseding artifact %q is not in this workspace", ErrInvalid, supersededBy)
		}
	}
	next := clone(a)
	next.Retired = &Retirement{At: s.now().UnixMilli(), By: by, SupersededBy: supersededBy, Reason: reason}
	if err = s.save(&next); err != nil {
		return Artifact{}, err
	}
	*a = next
	return clone(a), nil
}

// Restore brings a retired artifact back to the list. Restoring one that is
// not retired changes nothing.
func (s *Store) Restore(ws, id string) (Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	a, err := s.find(ws, id)
	if err != nil {
		return Artifact{}, err
	}
	if a.Retired == nil {
		return clone(a), nil
	}
	next := clone(a)
	next.Retired = nil
	if err = s.save(&next); err != nil {
		return Artifact{}, err
	}
	*a = next
	return clone(a), nil
}

func clone(a *Artifact) Artifact {
	out := *a
	out.Versions = append([]Version(nil), a.Versions...)
	out.Links = append([]Link(nil), a.Links...)
	if a.Retired != nil {
		retired := *a.Retired
		out.Retired = &retired
	}
	return out
}

func validateTodoList(content string) error {
	var list map[string]json.RawMessage
	var schema string
	var items []map[string]string
	if json.Unmarshal([]byte(content), &list) != nil || json.Unmarshal(list["schema"], &schema) != nil || schema != "todo-list.v1" || json.Unmarshal(list["items"], &items) != nil || items == nil {
		return fmt.Errorf("%w: expected todo-list.v1 and an items array", ErrInvalid)
	}
	seen := map[string]bool{}
	for _, item := range items {
		id := item["id"]
		if strings.TrimSpace(id) == "" || seen[id] || strings.TrimSpace(item["text"]) == "" || (item["status"] != "todo" && item["status"] != "done") {
			return fmt.Errorf("%w: items need unique ids, text and status todo or done", ErrInvalid)
		}
		seen[id] = true
	}
	return nil
}
