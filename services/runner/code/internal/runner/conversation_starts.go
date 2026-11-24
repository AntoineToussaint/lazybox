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

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/availability"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// An agent asks the person to start a conversation (docs/guides/orchestrator.md,
// "Ask an agent to start another conversation"): the person asked it, in its
// chat, to start one, and starting agents is theirs to allow. `lazybox
// conversation new` records the request on the asking conversation, where it
// waits like an approval (in the chat, the Inbox and a paired phone's
// notifications); the person starts it in one click (as asked, or edited) or
// declines. The conversation it starts is a peer in the same workspace (or a
// new one), not a supervised child: it shows in the Agents list and as a tab,
// and its first message is the brief with who asked for it. A conversation
// that holds an Orchestrator grant in force is not asked about: its request
// starts an agent under the grant, exactly as `lazybox agents spawn` does.
// Nothing an agent sends ever answers a request; there is no auto-approval.

// conversationBriefing is every coding agent's section on starting another
// conversation, a versioned section of its own (briefing.go). It names
// providers: an agent asked to hand work to Codex must know it can, and that
// neither its own sub-agent tool nor a subscription switch does that.
const conversationBriefing = `## Starting another conversation
To start another conversation (e.g. the person asks you to), run ` + "`lazybox conversation new --title \"…\" --brief \"…\"`" + ` (` + "`--brief -`" + ` reads it from stdin, ` + "`--brief-artifact ID`" + ` hands over an artifact, ` + "`--workspace new`" + ` gives it its own branches); the person approves it in one click.
- To hand work to another agent, including one of a different provider (e.g. Codex), use ` + "`lazybox conversation new --provider codex …`" + ` (` + "`--provider`" + ` takes claude, codex or an ACP agent; ` + "`--model`" + ` and ` + "`--profile`" + ` choose its model and subscription). Your own sub-agent tool only starts copies of yourself.
- Switching this conversation's subscription never changes its provider: it continues with the same agent on another account of that provider. To continue with another provider, start a conversation with ` + "`--provider`" + ` and hand over the context with ` + "`--brief-artifact ID`" + `.`

// Conversation start request states.
const (
	StartPending  = "pending"
	StartStarted  = "started"
	StartDeclined = "declined"
	StartExpired  = "expired"
)

const (
	// conversationStartTTL is how long a request waits for the person.
	conversationStartTTL = 24 * time.Hour
	// maxPendingConversationStarts bounds what one conversation has open at once…
	maxPendingConversationStarts = 3
	// …and maxConversationStartsPerHour how many it may ask for in an hour.
	maxConversationStartsPerHour = 10
	// MaxConversationStartWait bounds one WaitConversationStart call.
	MaxConversationStartWait = 60 * time.Second
	// conversationStartRetention keeps answered requests for a retried wait.
	conversationStartRetention = 7 * 24 * time.Hour
	maxStartTitle              = 200
	maxStartBrief              = 8 << 10
	maxStartReason             = 1000
)

// ErrConversationStartLimit refuses a request over the rate limits.
var ErrConversationStartLimit = errors.New("too many conversation requests")

// ErrInvalidConversationStart refuses a malformed request or answer.
var ErrInvalidConversationStart = errors.New("invalid conversation request")

// ConversationStartAsk is one `lazybox conversation new`.
type ConversationStartAsk struct {
	Session, Title, Brief, Artifact, Reason string
	// Workspace is "same" (default) or "new".
	Workspace                string
	Provider, Profile, Model string
	// StartFrom: with Workspace "new", the branch of origin its repositories
	// start from instead of the asking workspace's branches.
	StartFrom string
}

// ConversationStartAnswer is the person's answer: "start" with their edits
// (empty fields keep the agent's), or "decline" with a reason.
type ConversationStartAnswer struct {
	Decision, Reason                        string
	Title, Brief, Workspace, Profile, Model string
}

// ConversationStart is one agent's request that the person start a conversation.
type ConversationStart struct {
	ID           string `json:"id"`
	Session      string `json:"session"`
	Conversation string `json:"conversation"`
	// Workspace is the asking conversation's; Cwd its directory outside one.
	Workspace string `json:"workspace,omitempty"`
	Cwd       string `json:"cwd,omitempty"`
	// From is the asking conversation's title.
	From   string `json:"from"`
	Title  string `json:"title"`
	Brief  string `json:"brief,omitempty"`
	Reason string `json:"reason,omitempty"`
	// Mode is "same" or "new".
	Mode string `json:"mode"`
	// StartFrom: Mode "new" from this branch of origin, not the asking workspace's branches.
	StartFrom string `json:"startFrom,omitempty"`
	Provider  string `json:"provider"`
	Profile   string `json:"profile"`
	Model     string `json:"model,omitempty"`
	// Permission and Gateway are the asking conversation's: the new one gets no more.
	Permission      string `json:"permission,omitempty"`
	Gateway         string `json:"gateway,omitempty"`
	Artifact        string `json:"artifact,omitempty"`
	ArtifactTitle   string `json:"artifactTitle,omitempty"`
	ArtifactVersion uint32 `json:"artifactVersion,omitempty"`
	State           string `json:"state"`
	DeclineReason   string `json:"declineReason,omitempty"`
	// The conversation it started.
	StartedSession      string `json:"startedSession,omitempty"`
	StartedConversation string `json:"startedConversation,omitempty"`
	StartedWorkspace    string `json:"startedWorkspace,omitempty"`
	StartedTitle        string `json:"startedTitle,omitempty"`
	// Child: started under the asker's Orchestrator grant (never stored).
	Child        bool   `json:"-"`
	MissionTitle string `json:"-"`
	// SharedWith is how many conversations are live in Workspace (computed per view).
	SharedWith int       `json:"-"`
	CreatedAt  time.Time `json:"createdAt"`
	ExpiresAt  time.Time `json:"expiresAt"`
	ResolvedAt time.Time `json:"resolvedAt,omitzero"`
	// Told: the agent has the outcome, from its wait or a note.
	Told bool `json:"told,omitempty"`
}

// conversationStarts keeps every request, persisted so a request (and an
// agent waiting on it) outlives a Runner restart.
type conversationStarts struct {
	mu    sync.Mutex
	path  string
	items map[string]*ConversationStart
	// changed is closed and replaced whenever a request changes.
	changed chan struct{}
	// waiting counts the agents in WaitConversationStart, per request.
	waiting map[string]int
	// starting is a Start being opened: a second Start of it is refused.
	starting map[string]bool
	now      func() time.Time
	grace    time.Duration
}

func openConversationStarts(path string) (*conversationStarts, error) {
	s := &conversationStarts{path: path, items: map[string]*ConversationStart{}, changed: make(chan struct{}), waiting: map[string]int{}, starting: map[string]bool{}, now: time.Now, grace: repositoryNoteGrace}
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return s, nil
	}
	if err != nil {
		return nil, err
	}
	var saved []*ConversationStart
	if err = json.Unmarshal(data, &saved); err != nil {
		return nil, fmt.Errorf("conversation requests %s: %w", path, err)
	}
	for _, r := range saved {
		s.items[r.ID] = r
	}
	return s, nil
}

// saveLocked writes every request, dropping answered ones past retention.
func (s *conversationStarts) saveLocked() error {
	cutoff := s.now().Add(-conversationStartRetention)
	list := make([]*ConversationStart, 0, len(s.items))
	for id, r := range s.items {
		if r.State != StartPending && !r.ResolvedAt.IsZero() && r.ResolvedAt.Before(cutoff) {
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
	f, err := os.CreateTemp(filepath.Dir(s.path), ".conversation-starts-*")
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

func (s *conversationStarts) broadcastLocked() {
	close(s.changed)
	s.changed = make(chan struct{})
}

// expireLocked ends a request the person did not answer in time.
func (s *conversationStarts) expireLocked(r *ConversationStart) bool {
	if r.State != StartPending || s.now().Before(r.ExpiresAt) {
		return false
	}
	r.State, r.ResolvedAt = StartExpired, r.ExpiresAt
	return true
}

func (s *conversationStarts) get(id string) (ConversationStart, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	r := s.items[id]
	if r == nil {
		return ConversationStart{}, fmt.Errorf("conversation request %q: %w", id, os.ErrNotExist)
	}
	if s.expireLocked(r) {
		_ = s.saveLocked()
		s.broadcastLocked()
	}
	return *r, nil
}

// list returns the requests that pass keep, oldest first.
func (s *conversationStarts) list(keep func(*ConversationStart) bool) []ConversationStart {
	s.mu.Lock()
	defer s.mu.Unlock()
	expired := false
	var out []ConversationStart
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

// startText validates free text: bounded, valid UTF-8, no control characters
// (newlines and tabs only where line is false).
func startText(text string, limit int, what string, line bool) (string, error) {
	text = strings.TrimSpace(text)
	if len(text) > limit || !utf8.ValidString(text) {
		return "", fmt.Errorf("%w: the %s must be at most %d bytes of text", ErrInvalidConversationStart, what, limit)
	}
	if strings.IndexFunc(text, func(r rune) bool { return unicode.IsControl(r) && (line || (r != '\n' && r != '\t')) }) >= 0 {
		if line {
			return "", fmt.Errorf("%w: the %s is one line", ErrInvalidConversationStart, what)
		}
		return "", fmt.Errorf("%w: the %s contains control characters", ErrInvalidConversationStart, what)
	}
	return text, nil
}

func startMode(mode string) (string, error) {
	switch mode {
	case "", "same":
		return "same", nil
	case "new":
		return "new", nil
	}
	return "", fmt.Errorf("%w: the workspace is same or new", ErrInvalidConversationStart)
}

// startProfile is the subscription the new conversation runs on: the one
// named, else the asking agent's when the provider is its own (or none is
// named), else the provider's first.
func (b *Bench) startProfile(asker accounts.Profile, provider, profile string) (accounts.Profile, error) {
	profiles := b.Profiles()
	if profile != "" {
		for _, p := range profiles {
			if p.ID == profile {
				if provider != "" && p.Provider != provider {
					return accounts.Profile{}, fmt.Errorf("%w: subscription %q is %s's, not %s's", ErrInvalidConversationStart, profile, p.Provider, provider)
				}
				return p, nil
			}
		}
		return accounts.Profile{}, fmt.Errorf("%w: no subscription %q", ErrInvalidConversationStart, profile)
	}
	if provider == "" || provider == asker.Provider {
		return asker, nil
	}
	var have []string
	for _, p := range profiles {
		if p.Provider == provider {
			return p, nil
		}
		if !slices.Contains(have, p.Provider) {
			have = append(have, p.Provider)
		}
	}
	return accounts.Profile{}, fmt.Errorf("%w: no %s subscription on this computer (there are: %s)", ErrInvalidConversationStart, provider, strings.Join(have, ", "))
}

// liveConversations counts the conversations live in a workspace now.
func (b *Bench) liveConversations(workspace string) int {
	if workspace == "" {
		return 0
	}
	seen := map[string]bool{}
	for _, e := range b.List() {
		if e.CollectionID != workspace || e.Archived || agent.IsAssistant(e.Purpose) || e.Engine == nil || e.Engine.Snapshot().Closed {
			continue
		}
		seen[conversationOf(e)] = true
	}
	return len(seen)
}

// ConversationStartSession returns the execution that made a request (for
// the transport's agent allowlist).
func (b *Bench) ConversationStartSession(id string) (string, error) {
	r, err := b.conversationStarts.get(id)
	if err != nil {
		return "", err
	}
	return r.Session, nil
}

// GetConversationStart returns one request.
func (b *Bench) GetConversationStart(id string) (ConversationStart, error) {
	r, err := b.conversationStarts.get(id)
	if err == nil {
		r.SharedWith = b.liveConversations(r.Workspace)
	}
	return r, err
}

// RequestConversationStart records an agent's request that the person start
// a conversation, on the asking conversation. It starts nothing, unless the
// conversation holds an Orchestrator grant in force: then the agent starts at
// once under it. A request with the same title that still waits is returned
// rather than repeated.
func (b *Bench) RequestConversationStart(ctx context.Context, ask ConversationStartAsk) (ConversationStart, error) {
	e, err := b.Get(ask.Session)
	if err != nil {
		return ConversationStart{}, err
	}
	if e, err = b.latest(e); err != nil {
		return ConversationStart{}, err
	}
	if t := agent.TypeOf(e.Purpose); !t.Delegates {
		return ConversationStart{}, fmt.Errorf("%w: a conversation of type %s cannot start conversations", ErrInvalidConversationStart, t.Label)
	}
	r := ConversationStart{Session: e.ID, Conversation: conversationOf(e), Workspace: e.CollectionID, Cwd: e.Cwd, From: b.agentTitle(ctx, e), State: StartPending,
		Permission: e.Options.Permission, Gateway: e.Saved.GatewayID}
	if r.Title, err = startText(ask.Title, maxStartTitle, "title", true); err != nil {
		return ConversationStart{}, err
	}
	if r.Title == "" {
		return ConversationStart{}, fmt.Errorf("%w: give the conversation a --title", ErrInvalidConversationStart)
	}
	if r.Brief, err = startText(ask.Brief, maxStartBrief, "brief", false); err != nil {
		return ConversationStart{}, fmt.Errorf("%w (publish a longer one as an artifact and pass --brief-artifact)", err)
	}
	if r.Reason, err = startText(ask.Reason, maxStartReason, "reason", false); err != nil {
		return ConversationStart{}, err
	}
	if r.Mode, err = startMode(ask.Workspace); err != nil {
		return ConversationStart{}, err
	}
	if r.Mode == "new" && (e.CollectionID == "" || b.Collections == nil) {
		return ConversationStart{}, fmt.Errorf("%w: --workspace new needs a workspace to branch from; this conversation has none", ErrInvalidConversationStart)
	}
	if r.StartFrom = strings.TrimSpace(ask.StartFrom); r.StartFrom != "" {
		if r.Mode != "new" {
			return ConversationStart{}, fmt.Errorf("%w: --start-from needs --workspace new (this workspace's files are already on their branches)", ErrInvalidConversationStart)
		}
		if len(r.StartFrom) > 255 || strings.HasPrefix(r.StartFrom, "-") || strings.ContainsAny(r.StartFrom, " \t\n\x00~^:?*[\\") {
			return ConversationStart{}, fmt.Errorf("%w: --start-from %q is not a branch name", ErrInvalidConversationStart, r.StartFrom)
		}
	}
	profile, err := b.startProfile(e.Profile, strings.TrimSpace(ask.Provider), strings.TrimSpace(ask.Profile))
	if err != nil {
		return ConversationStart{}, err
	}
	r.Profile, r.Provider, r.Model = profile.ID, profile.Provider, strings.TrimSpace(ask.Model)
	if r.Model == "" && profile.Provider == e.Profile.Provider {
		r.Model = e.Options.Model
	}
	if r.Model, err = availability.ResolveModel(r.Provider, r.Model); err != nil {
		return ConversationStart{}, err
	}
	if id := strings.TrimSpace(ask.Artifact); id != "" {
		if e.CollectionID == "" || b.Artifacts == nil {
			return ConversationStart{}, fmt.Errorf("%w: this conversation has no workspace artifacts", ErrInvalidConversationStart)
		}
		doc, err := b.Artifacts.Get(e.CollectionID, id, 0)
		if err != nil {
			return ConversationStart{}, fmt.Errorf("%w: no artifact %q in this workspace (lazybox artifact list)", ErrInvalidConversationStart, id)
		}
		r.Artifact, r.ArtifactTitle, r.ArtifactVersion = doc.ID, doc.Title, doc.ContentVersion
	}
	if r.Brief == "" && r.Artifact == "" {
		return ConversationStart{}, fmt.Errorf("%w: give a --brief (what to do, done when, boundaries) or a --brief-artifact", ErrInvalidConversationStart)
	}
	if c, err := b.orchestrationCaller(e.ID); err == nil && c.holds {
		return b.startUnderGrant(ctx, e, r, ask)
	}

	shared := b.liveConversations(r.Workspace)
	s := b.conversationStarts
	now := s.now()
	r.CreatedAt, r.ExpiresAt = now, now.Add(conversationStartTTL)
	s.mu.Lock()
	defer s.mu.Unlock()
	pending, recent := 0, 0
	for _, other := range s.items {
		s.expireLocked(other)
		if other.Conversation != r.Conversation {
			continue
		}
		if other.State == StartPending {
			if strings.EqualFold(other.Title, r.Title) {
				out := *other
				out.SharedWith = shared
				return out, nil
			}
			pending++
		}
		if now.Sub(other.CreatedAt) < time.Hour {
			recent++
		}
	}
	if pending >= maxPendingConversationStarts {
		return ConversationStart{}, fmt.Errorf("%w: %d requests to start a conversation already wait for the person here; wait for their answer", ErrConversationStartLimit, pending)
	}
	if recent >= maxConversationStartsPerHour {
		return ConversationStart{}, fmt.Errorf("%w: at most %d an hour from one conversation", ErrConversationStartLimit, maxConversationStartsPerHour)
	}
	r.ID = "start-" + strings.ToLower(commands.ID())
	s.items[r.ID] = &r
	if err := s.saveLocked(); err != nil {
		delete(s.items, r.ID)
		return ConversationStart{}, err
	}
	s.broadcastLocked()
	out := r
	out.SharedWith = shared
	return out, nil
}

// startUnderGrant starts the agent under the asking conversation's
// Orchestrator grant, as `lazybox agents spawn` does: a child on its mission.
func (b *Bench) startUnderGrant(ctx context.Context, e *Execution, r ConversationStart, ask ConversationStartAsk) (ConversationStart, error) {
	brief := r.Brief
	if r.Artifact != "" {
		ref, err := b.artifactBrief(r, r.Mode == "new")
		if err != nil {
			return ConversationStart{}, err
		}
		brief = strings.TrimSpace(brief + "\n\n" + ref)
	}
	spawn := SpawnRequest{Session: e.ID, Title: r.Title, Brief: brief, Model: r.Model, Workspace: r.Mode, StartFrom: r.StartFrom}
	if ask.Profile != "" || ask.Provider != "" {
		spawn.Profile = r.Profile
	}
	view, err := b.Spawn(ctx, spawn)
	if err != nil {
		return ConversationStart{}, err
	}
	r.State, r.Child, r.ResolvedAt, r.Told = StartStarted, true, time.Now(), true
	if view.Mission != nil {
		r.MissionTitle = view.Mission.Title
	}
	if s := view.Spawned; s != nil {
		r.StartedSession, r.StartedConversation, r.StartedWorkspace, r.StartedTitle = s.Session, s.Conversation, s.Workspace, s.Title
	}
	return r, nil
}

// artifactBrief is the line that hands the new conversation its brief
// artifact. Inline (a workspace of its own cannot read the asking one's
// artifacts), it is the artifact's text.
func (b *Bench) artifactBrief(r ConversationStart, inline bool) (string, error) {
	if inline {
		doc, err := b.Artifacts.Get(r.Workspace, r.Artifact, r.ArtifactVersion)
		if err != nil {
			return "", err
		}
		return fmt.Sprintf("Your brief is the Lazybox artifact “%s” (version %d):\n\n%s", doc.Title, doc.ContentVersion, doc.Content), nil
	}
	return fmt.Sprintf("Your brief is the Lazybox artifact “%s” (id %s, version %d): read it with `lazybox artifact get %s --version %d` before you start, and keep it current with `lazybox artifact put … --id %s` if the plan changes.", r.ArtifactTitle, r.Artifact, r.ArtifactVersion, r.Artifact, r.ArtifactVersion, r.Artifact), nil
}

// WaitConversationStart waits, at most timeout, for the person's answer. An
// answer collected here is the agent's: no note is sent for it, and the
// asking conversation shows one line for it.
func (b *Bench) WaitConversationStart(ctx context.Context, id string, timeout time.Duration) (ConversationStart, error) {
	timeout = min(max(timeout, 0), MaxConversationStartWait)
	s := b.conversationStarts
	s.mu.Lock()
	s.waiting[id]++
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		if s.waiting[id]--; s.waiting[id] <= 0 {
			delete(s.waiting, id)
		}
		// Answered as this wait ended, before it saw it: the agent is told instead.
		missed := s.waiting[id] == 0 && s.items[id] != nil && !s.items[id].Told && (s.items[id].State == StartStarted || s.items[id].State == StartDeclined)
		s.mu.Unlock()
		if missed {
			b.startNoteLater(id)
		}
	}()
	deadline := time.NewTimer(timeout)
	defer deadline.Stop()
	for {
		r, err := b.GetConversationStart(id)
		if err != nil {
			return r, err
		}
		s.mu.Lock()
		changed := s.changed
		collected := false
		if current := s.items[id]; current != nil && (current.State == StartStarted || current.State == StartDeclined) && !current.Told {
			current.Told, collected = true, true
			_ = s.saveLocked()
			r.Told = true
		}
		s.mu.Unlock()
		if collected {
			b.recordStartLine(r)
		}
		if r.State != StartPending {
			return r, nil
		}
		select {
		case <-changed:
		case <-deadline.C:
			return r, nil
		case <-ctx.Done():
			return r, ctx.Err()
		case <-b.ctx.Done():
			return r, errors.New("the Runner is stopping; wait again in a moment")
		}
	}
}

// AnswerConversationStart is the person's answer. Start opens the
// conversation (with the person's edits) and gives it its brief; decline
// tells the agent why. Either is answered once.
func (b *Bench) AnswerConversationStart(ctx context.Context, id string, a ConversationStartAnswer) (ConversationStart, error) {
	switch a.Decision {
	case "decline":
		reason, err := startText(a.Reason, maxStartReason, "reason", false)
		if err != nil {
			return ConversationStart{}, err
		}
		return b.resolveConversationStart(id, func(r *ConversationStart) { r.State, r.DeclineReason = StartDeclined, reason })
	case "start":
	default:
		return ConversationStart{}, fmt.Errorf("%w: the decision is start or decline", ErrInvalidConversationStart)
	}
	s := b.conversationStarts
	s.mu.Lock()
	current := s.items[id]
	if current == nil {
		s.mu.Unlock()
		return ConversationStart{}, fmt.Errorf("conversation request %q: %w", id, os.ErrNotExist)
	}
	s.expireLocked(current)
	if current.State != StartPending {
		state := current.State
		s.mu.Unlock()
		return ConversationStart{}, fmt.Errorf("this request was already %s", state)
	}
	if s.starting[id] {
		s.mu.Unlock()
		return ConversationStart{}, errors.New("this conversation is being started")
	}
	s.starting[id] = true
	r := *current
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.starting, id)
		s.mu.Unlock()
	}()
	if err := b.editConversationStart(&r, a); err != nil {
		return ConversationStart{}, err
	}
	started, err := b.startPeer(ctx, r)
	if err != nil {
		return ConversationStart{}, err
	}
	return b.resolveConversationStart(id, func(current *ConversationStart) {
		current.Title, current.Brief, current.Mode, current.StartFrom, current.Profile, current.Provider, current.Model = r.Title, r.Brief, r.Mode, r.StartFrom, r.Profile, r.Provider, r.Model
		current.State, current.StartedSession, current.StartedConversation, current.StartedWorkspace, current.StartedTitle = StartStarted, started.ID, conversationOf(started), started.CollectionID, r.Title
	})
}

// editConversationStart applies the person's edits to a request.
func (b *Bench) editConversationStart(r *ConversationStart, a ConversationStartAnswer) error {
	var err error
	if strings.TrimSpace(a.Title) != "" {
		if r.Title, err = startText(a.Title, maxStartTitle, "title", true); err != nil {
			return err
		}
	}
	if strings.TrimSpace(a.Brief) != "" {
		if r.Brief, err = startText(a.Brief, maxStartBrief, "brief", false); err != nil {
			return err
		}
	}
	if a.Workspace != "" {
		if r.Mode, err = startMode(a.Workspace); err != nil {
			return err
		}
		if r.Mode == "new" && r.Workspace == "" {
			return fmt.Errorf("%w: a new workspace needs one to branch from; the asking conversation has none", ErrInvalidConversationStart)
		}
		if r.Mode != "new" {
			r.StartFrom = "" // this workspace's files: there is nothing to start
		}
	}
	if a.Profile != "" && a.Profile != r.Profile {
		profile, err := b.startProfile(accounts.Profile{}, "", a.Profile)
		if err != nil {
			return err
		}
		if profile.Provider != r.Provider && a.Model == "" {
			r.Model = "" // another agent: its own default model
		}
		r.Profile, r.Provider = profile.ID, profile.Provider
	}
	if a.Model != "" {
		r.Model = a.Model
	}
	model, err := availability.ResolveModel(r.Provider, r.Model)
	if err != nil {
		return err
	}
	r.Model = model
	return nil
}

// startPeer opens the conversation a request asks for and gives it its
// brief. It runs at the asking conversation's permission, through its
// gateway, in its workspace or a new one branched from it.
func (b *Bench) startPeer(ctx context.Context, r ConversationStart) (*Execution, error) {
	options := accounts.Options{Model: r.Model, Permission: r.Permission}
	if err := options.Validate(); err != nil {
		return nil, fmt.Errorf("%w: %v", ErrInvalidConversationStart, err)
	}
	workspace := r.Workspace
	if r.Mode == "new" {
		if b.Collections == nil || r.Workspace == "" {
			return nil, fmt.Errorf("%w: a new workspace needs one to branch from", ErrInvalidConversationStart)
		}
		created, err := b.childWorkspace(ctx, r.Workspace, r.Title, "conversation-"+r.ID, r.StartFrom)
		if err != nil {
			return nil, err
		}
		workspace = created
	}
	brief := r.Brief
	if r.Artifact != "" && b.Artifacts != nil {
		held := r
		if workspace != r.Workspace {
			// A workspace of its own reads only its own artifacts: the brief goes with it.
			copied, err := b.copyArtifact(r, workspace)
			if err != nil {
				return nil, err
			}
			held.Artifact, held.ArtifactVersion = copied.ID, copied.ContentVersion
		}
		ref, _ := b.artifactBrief(held, false)
		brief = strings.TrimSpace(brief + "\n\n" + ref)
	}
	var e *Execution
	var err error
	if workspace != "" {
		e, err = b.OpenCollection(r.Profile, workspace, "", options, r.Gateway, "")
	} else {
		e, err = b.Open(r.Profile, r.Cwd, "", options, r.Gateway, "", "")
	}
	if err != nil {
		return nil, err
	}
	_ = b.History.Rename(ctx, e.ID, r.Title)
	why := ""
	if r.Reason != "" {
		why = ": " + r.Reason
	}
	text := r.Title + "\n\n" + brief + fmt.Sprintf("\n\n(The conversation “%s” asked to start this one, at the person's request%s. The person approved this brief: it is your task, and you answer to them here. You are a separate conversation, not its sub-agent.)", r.From, why)
	origin := agent.Origin{Kind: agent.OriginConversationBrief, Summary: "Started from “" + r.From + "” at your request: " + r.Title, Detail: r.Reason, From: r.From, Conversation: r.Conversation, Session: r.Session}
	if _, err := b.deliverAgentMessage(ctx, e, "conversation-start-"+r.ID, text, origin, work.PromptSource{Kind: work.SourceAgent}); err != nil {
		// The conversation stands; the person sees it has no brief and can paste one.
		b.recordLineageEvent(e, "conversation_started", "Its brief was not delivered: "+err.Error(), map[string]any{"conversation": r.Conversation, "session": r.Session})
	}
	return e, nil
}

// copyArtifact puts the brief artifact's approved version in another workspace.
func (b *Bench) copyArtifact(r ConversationStart, workspace string) (artifacts.Document, error) {
	doc, err := b.Artifacts.Get(r.Workspace, r.Artifact, r.ArtifactVersion)
	if err != nil {
		return artifacts.Document{}, err
	}
	return b.Artifacts.Put(workspace, artifacts.Put{Title: doc.Title, Kind: doc.Kind, Format: doc.Format, Content: doc.Content,
		Author: artifacts.Author{Kind: "agent", SessionID: r.Session}, Source: artifacts.Source{Kind: "agent", SessionID: r.Session, ConversationID: r.Conversation}})
}

// resolveConversationStart applies an answer, wakes a waiting agent, and
// otherwise sends the answer into the conversation once no wait collects it.
func (b *Bench) resolveConversationStart(id string, answer func(*ConversationStart)) (ConversationStart, error) {
	s := b.conversationStarts
	s.mu.Lock()
	r := s.items[id]
	if r == nil {
		s.mu.Unlock()
		return ConversationStart{}, fmt.Errorf("conversation request %q: %w", id, os.ErrNotExist)
	}
	s.expireLocked(r)
	if r.State != StartPending {
		state := r.State
		s.mu.Unlock()
		return ConversationStart{}, fmt.Errorf("this request was already %s", state)
	}
	before := *r
	answer(r)
	r.ResolvedAt = s.now()
	if err := s.saveLocked(); err != nil {
		*r = before
		s.mu.Unlock()
		return before, err
	}
	s.broadcastLocked()
	out := *r
	s.mu.Unlock()
	b.startNoteLater(id)
	out.SharedWith = b.liveConversations(out.Workspace)
	return out, nil
}

// startNoteLater tells the asking agent the answer unless a waiting agent
// collects it first; either way the asking conversation shows one line.
func (b *Bench) startNoteLater(id string) {
	s := b.conversationStarts
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
		if r == nil || r.Told || (r.State != StartStarted && r.State != StartDeclined) || s.waiting[id] > 0 {
			s.mu.Unlock()
			return
		}
		r.Told = true
		_ = s.saveLocked()
		out := *r
		s.mu.Unlock()
		if !b.deliverStartNote(out) {
			b.recordStartLine(out)
		}
	}()
}

// startOutcome is the answer in the asking conversation: the line it shows,
// and the note its agent is told when it no longer waits.
func startOutcome(r ConversationStart) (kind, line, note string) {
	switch r.State {
	case StartStarted:
		where := "in this workspace"
		if r.Mode == "new" {
			where = "in a new workspace"
		}
		return agent.OriginConversationStarted, fmt.Sprintf("Started “%s” · %s · you approved", r.StartedTitle, where),
			fmt.Sprintf("[Lazybox] The person started the conversation you asked for: “%s” (conversation %s, %s). It works on its own and answers to them; do not do its work here.", r.StartedTitle, r.StartedConversation, where)
	case StartDeclined:
		why, said := "", ""
		if r.DeclineReason != "" {
			why, said = " — “"+r.DeclineReason+"”", " Their reason: "+r.DeclineReason
		}
		return agent.OriginConversationDeclined, fmt.Sprintf("You declined starting “%s”%s", r.Title, why),
			fmt.Sprintf("[Lazybox] The person declined starting the conversation “%s”.%s Do not start it another way; continue here, or ask them how to proceed.", r.Title, said)
	}
	return "", "", ""
}

// recordStartLine shows the answer as one quiet line in the asking conversation.
func (b *Bench) recordStartLine(r ConversationStart) {
	kind, line, _ := startOutcome(r)
	if kind == "" {
		return
	}
	e, err := b.Get(r.Session)
	if err != nil {
		return
	}
	data := map[string]any{"request": r.ID}
	if r.State == StartStarted {
		data["conversation"], data["session"] = r.StartedConversation, r.StartedSession
	}
	b.recordLineageEvent(e, kind, line, data)
}

// deliverStartNote sends the answer to the asking conversation's live agent
// as a message (queued while it works), shown as that answer's line.
func (b *Bench) deliverStartNote(r ConversationStart) bool {
	kind, line, note := startOutcome(r)
	if kind == "" {
		return false
	}
	target, err := b.Get(r.Session)
	if err == nil {
		target, err = b.latest(target)
	}
	if err != nil || target.Engine.Snapshot().Closed {
		return false
	}
	origin := agent.Origin{Kind: kind, Summary: line, Detail: r.DeclineReason}
	if r.State == StartStarted {
		origin.Conversation, origin.Session = r.StartedConversation, r.StartedSession
	}
	ctx, cancel := context.WithTimeout(b.ctx, 10*time.Second)
	defer cancel()
	id := "conversation-note-" + r.ID
	receipt, err := target.Engine.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: note, Origin: &origin})
	if err != nil || receipt.Error != "" {
		return false
	}
	b.RecordPrompt(target, id, note, work.PromptSource{Kind: work.SourceAgent})
	return true
}

// PendingConversationStarts are the requests waiting in e's conversation.
func (b *Bench) PendingConversationStarts(e *Execution) []ConversationStart {
	if b.conversationStarts == nil {
		return nil
	}
	conversation := conversationOf(e)
	out := b.conversationStarts.list(func(r *ConversationStart) bool {
		return r.Conversation == conversation && r.State == StartPending
	})
	for i := range out {
		out[i].SharedWith = b.liveConversations(out[i].Workspace)
	}
	return out
}
