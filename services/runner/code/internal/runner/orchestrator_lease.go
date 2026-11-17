package runner

import (
	"context"
	"fmt"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Leases, live overlap and review capacity (work/lease.go; sota-agents.md
// §6.5–6.6). Leases are advisory and nothing is blocked mid-edit: after
// every turn of a mission's attempt, what the turn changed (its code
// checkpoint) is intersected with what the mission's other live attempts
// changed and with other tasks' exclusive leases; an overlap is recorded once
// and its orchestrator told. New agents are held while as many tasks wait for
// the person as the grant allows.

// LeaseRequest is one `lazybox lease claim`.
type LeaseRequest struct {
	Session, Task, Path, Reason string
	Exclusive                   bool
	TTL                         time.Duration
}

// leaseTask is the mission task a caller leases for: its own live attempt's,
// or (an orchestrator) a task of an agent it started.
func (b *Bench) leaseTask(session, task string) (*work.State, *work.Task, orchestrationCaller, error) {
	c, err := b.orchestrationCaller(session)
	if err != nil {
		return nil, nil, c, err
	}
	for _, at := range b.Work.Attempts() {
		if at.Attempt.Conversation == c.conversation && (task == "" || task == at.Task) {
			state, err := b.Work.Mission(at.Mission)
			if err != nil {
				return nil, nil, c, err
			}
			return state, state.Task(at.Task), c, nil
		}
	}
	if task != "" && c.holds {
		if t := c.held.Mission.Task(task); t != nil {
			return c.held.Mission, t, c, nil
		}
	}
	return nil, nil, c, fmt.Errorf("%w: this conversation works on no mission task; leases belong to a task (an orchestrator names one with --task)", work.ErrRefused)
}

// ClaimLease records an advisory lease on a path for a task.
func (b *Bench) ClaimLease(ctx context.Context, r LeaseRequest) (*work.State, *work.Lease, error) {
	state, task, c, err := b.leaseTask(r.Session, r.Task)
	if err != nil {
		return nil, nil, err
	}
	mode := work.LeaseShared
	if r.Exclusive {
		mode = work.LeaseExclusive
	}
	actor := work.Actor{Kind: work.ActorAgent, Conversation: c.conversation, Execution: c.e.ID}
	next, id, err := b.Work.ClaimLease(state.ID, actor, "", work.LeasePayload{Task: task.ID, Path: r.Path, Mode: mode, TTL: int64(r.TTL / time.Second), Reason: r.Reason})
	if err != nil {
		return nil, nil, err
	}
	b.orch().broadcast()
	for _, l := range next.Leases {
		if l.Lease == id {
			return next, l, nil
		}
	}
	return next, nil, nil
}

// ReleaseLease ends one of the caller's task's leases (by id or path).
func (b *Bench) ReleaseLease(ctx context.Context, session, lease string) (*work.State, error) {
	c, err := b.orchestrationCaller(session)
	if err != nil {
		return nil, err
	}
	for _, m := range b.Work.Missions() {
		if m.Err != nil {
			continue
		}
		for _, l := range m.State.LiveLeases(time.Now()) {
			if l.Lease == lease || l.Path == lease {
				state, err := b.Work.ReleaseLease(m.State.ID, l.Lease, work.Actor{Kind: work.ActorAgent, Conversation: c.conversation, Execution: c.e.ID})
				if err == nil {
					b.orch().broadcast()
				}
				return state, err
			}
		}
	}
	return nil, fmt.Errorf("%w: no lease %q is held", work.ErrRefused, lease)
}

// Leases lists the live leases of the mission the caller works in.
func (b *Bench) Leases(ctx context.Context, session string) (*work.State, []*work.Lease, error) {
	c, err := b.orchestrationCaller(session)
	if err != nil {
		return nil, nil, err
	}
	var state *work.State
	switch {
	case c.holds:
		state = c.held.Mission
	case c.isChild:
		state = c.parent.Mission
	default:
		if s, _, _, err := b.leaseTask(session, ""); err == nil {
			state = s
		}
	}
	if state == nil {
		return nil, nil, fmt.Errorf("%w: this conversation works on no mission", work.ErrRefused)
	}
	return state, state.LiveLeases(time.Now()), nil
}

// ---- Live overlap -------------------------------------------------------------

// touched is what each conversation's turns changed ("repository/path"),
// kept as turns end and read back from its checkpoints after a restart.
type touched struct {
	mu    sync.Mutex
	files map[string]map[string]bool
	// seeded: the conversation's earlier turns were read.
	seeded map[string]bool
}

// maxSeedTurns bounds how far back a conversation's checkpoints are read.
const maxSeedTurns = 40

func (b *Bench) turnFiles(ctx context.Context, conversation string, seq int) []string {
	repos, _, err := b.Checkpoints.Changes(ctx, conversation, seq)
	if err != nil {
		return nil
	}
	var out []string
	for _, r := range repos {
		for _, f := range r.Files {
			out = append(out, r.Member+"/"+f.Path)
		}
	}
	return out
}

// filesOf is every file a conversation's turns changed (the newest turns).
func (b *Bench) filesOf(ctx context.Context, conversation string) map[string]bool {
	touchedFiles := b.orch().touched
	touchedFiles.mu.Lock()
	seeded := touchedFiles.seeded[conversation]
	touchedFiles.mu.Unlock()
	if !seeded && b.Checkpoints != nil {
		if rec, err := b.Checkpoints.List(conversation); err == nil {
			turns := rec.Turns
			if len(turns) > maxSeedTurns {
				turns = turns[len(turns)-maxSeedTurns:]
			}
			var files []string
			for _, t := range turns {
				if t.After != nil && t.Files > 0 {
					files = append(files, b.turnFiles(ctx, conversation, t.Seq)...)
				}
			}
			touchedFiles.add(conversation, files)
		}
		touchedFiles.mu.Lock()
		touchedFiles.seeded[conversation] = true
		touchedFiles.mu.Unlock()
	}
	touchedFiles.mu.Lock()
	defer touchedFiles.mu.Unlock()
	out := map[string]bool{}
	for f := range touchedFiles.files[conversation] {
		out[f] = true
	}
	return out
}

func (t *touched) add(conversation string, files []string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.files[conversation] == nil {
		t.files[conversation] = map[string]bool{}
	}
	for _, f := range files {
		t.files[conversation][f] = true
	}
}

// overlapTurnEnded checks a turn that ended (after its checkpoint) against
// the mission's other live attempts and exclusive leases.
func (b *Bench) overlapTurnEnded(e *Execution) {
	if b.Work == nil || b.Checkpoints == nil {
		return
	}
	conversation := conversationOf(e)
	var mine []work.Attachment
	for _, at := range b.Work.Attempts() {
		if at.Attempt.Conversation == conversation {
			mine = append(mine, at)
		}
	}
	if len(mine) == 0 {
		return
	}
	ctx, cancel := context.WithTimeout(b.ctx, 30*time.Second)
	defer cancel()
	if rec, err := b.Checkpoints.List(conversation); err == nil && len(rec.Turns) > 0 {
		last := rec.Turns[len(rec.Turns)-1]
		b.orch().touched.add(conversation, b.turnFiles(ctx, conversation, last.Seq))
	}
	b.DetectOverlaps(ctx, conversation)
}

// DetectOverlaps records the overlaps between a conversation's task and the
// rest of its mission, and tells the orchestrators of the tasks involved.
func (b *Bench) DetectOverlaps(ctx context.Context, conversation string) {
	files := b.filesOf(ctx, conversation)
	if len(files) == 0 {
		return
	}
	for _, at := range b.Work.Attempts() {
		if at.Attempt.Conversation != conversation {
			continue
		}
		state, err := b.Work.Mission(at.Mission)
		if err != nil {
			continue
		}
		for _, other := range b.Work.Attempts() {
			if other.Mission != at.Mission || other.Task == at.Task || other.Attempt.Conversation == conversation {
				continue
			}
			theirs := b.filesOf(ctx, other.Attempt.Conversation)
			var shared []string
			for f := range files {
				if theirs[f] {
					shared = append(shared, f)
				}
			}
			if len(shared) > 0 {
				b.recordOverlap(state, work.OverlapPayload{Tasks: []string{at.Task, other.Task}, Paths: capPaths(shared), Kind: "files"})
			}
		}
		for _, l := range state.LiveLeases(time.Now()) {
			if l.Task == at.Task || l.Mode != work.LeaseExclusive {
				continue
			}
			var inside []string
			for f := range files {
				if work.PathMatch(l.Path, f) {
					inside = append(inside, f)
				}
			}
			if len(inside) > 0 {
				b.recordOverlap(state, work.OverlapPayload{Tasks: []string{at.Task, l.Task}, Paths: capPaths(inside), Kind: "lease", Lease: l.Lease})
			}
		}
	}
}

func capPaths(paths []string) []string {
	sort.Strings(paths)
	if len(paths) > 20 {
		paths = paths[:20]
	}
	return paths
}

func (b *Bench) recordOverlap(state *work.State, o work.OverlapPayload) {
	next, wrote, err := b.Work.RecordOverlap(state.ID, o)
	if err != nil || !wrote {
		return
	}
	b.orch().broadcast()
	titles := []string{}
	for _, id := range o.Tasks {
		if t := next.Task(id); t != nil {
			titles = append(titles, "“"+t.Title+"”")
		}
	}
	what := "both changed"
	if o.Kind == "lease" {
		what = "one changed what the other leased exclusively:"
	}
	text := fmt.Sprintf("%s %s %s", strings.Join(titles, " and "), what, strings.Join(o.Paths, ", "))
	// Each orchestrator of the tasks decides: sequence them, merge or hand off.
	told := map[string]bool{}
	for _, id := range o.Tasks {
		t := next.Task(id)
		if t == nil || t.Current() == nil || t.Current().Parent == "" || told[t.Current().Parent] {
			continue
		}
		parent := t.Current().Parent
		told[parent] = true
		e, err := b.Get(parent)
		if err != nil {
			continue
		}
		b.recordLineageEvent(e, "overlap_detected", "Overlap: "+text, map[string]any{"mission": next.ID, "tasks": o.Tasks})
		ctx, cancel := context.WithTimeout(b.ctx, 10*time.Second)
		message := "[Lazybox] Your agents overlap: " + text + ". Nothing was blocked. Decide: sequence them (stop one and start it again after the other is accepted), have one hand its change to the other, or tell the person."
		_, _ = b.deliverAgentMessage(ctx, e, "overlap-"+strings.Join(o.Tasks, "-")+"-"+fmt.Sprint(len(next.Overlaps)), message, lazyboxOrigin(agent.OriginOverlap, "Overlap: "+text, o.Kind), work.PromptSource{Kind: work.SourceMission, Mission: next.ID})
		cancel()
	}
}

// ---- Review capacity ----------------------------------------------------------

// checkReview holds a spawn while as many tasks wait for the person as the
// grant allows: starting more work is the person's decision.
func (b *Bench) checkReview(state *work.State, g *work.Grant, orchestrator *Execution) error {
	limit := g.Ceiling.ReviewLimit
	if limit <= 0 {
		limit = work.DefaultReviewLimit
	}
	waiting := state.AwaitingReview()
	if waiting < limit {
		return nil
	}
	if _, wrote, _ := b.Work.RecordHold(state.ID, work.HoldPayload{Grant: g.Grant, Waiting: waiting, Limit: limit}); wrote {
		b.recordLineageEvent(orchestrator, "wip_held", fmt.Sprintf("New agents held: %s for your review", plural(waiting, "task waits", "tasks wait")), map[string]any{"mission": state.ID, "grant": g.Grant})
	}
	return fmt.Errorf("%w: %s for the person's review (the grant holds new agents at %d). Wait for them to accept or send back work (lazybox agents wait --until settled), or finish with the agents you have", work.ErrRefused, plural(waiting, "task waits", "tasks wait"), limit)
}
