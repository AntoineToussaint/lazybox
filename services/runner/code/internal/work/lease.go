package work

import (
	"slices"
	"sort"
	"strings"
	"time"
)

// Conflict avoidance (sota-agents.md §6.5–6.6): lease, detect, serialise.
// A lease is advisory: a task says which paths it will change, exclusively
// or not. Nothing is blocked mid-edit; instead, after every turn the Runner
// intersects what each live attempt of a mission changed (per-turn
// checkpoints) and records an overlap when two tasks touched the same file
// or one edited inside another's exclusive lease. And work is sized to
// review: when as many tasks wait for the person as the grant allows, new
// agents are held.

const (
	LeaseClaimed    = "lease.claimed"
	LeaseReleased   = "lease.released"
	OverlapDetected = "overlap.detected"
	WIPHeld         = "wip.held"
)

// Lease modes.
const (
	LeaseExclusive = "exclusive"
	LeaseShared    = "shared"
)

// DefaultLeaseTTL and MaxLeaseTTL bound a lease; it lapses by itself.
const (
	DefaultLeaseTTL = 2 * time.Hour
	MaxLeaseTTL     = 24 * time.Hour
	// DefaultReviewLimit is how many tasks may wait for the person before an
	// orchestrator's new agents are held.
	DefaultReviewLimit = 3
)

type (
	// LeasePayload is lease.claimed (and, with only Lease, lease.released).
	LeasePayload struct {
		Lease  string `json:"lease"`
		Task   string `json:"task,omitempty"`
		Path   string `json:"path,omitempty"`
		Mode   string `json:"mode,omitempty"`
		TTL    int64  `json:"ttl_seconds,omitempty"`
		Reason string `json:"reason,omitempty"`
	}
	// OverlapPayload is overlap.detected: two tasks touched the same paths.
	OverlapPayload struct {
		Tasks []string `json:"tasks"`
		Paths []string `json:"paths"`
		// Kind is files (both changed them) or lease (one changed what the
		// other leased exclusively).
		Kind string `json:"kind"`
		// Lease, for kind lease.
		Lease string `json:"lease,omitempty"`
	}
	// HoldPayload is wip.held: new agents held because review is full.
	HoldPayload struct {
		Grant   string `json:"grant"`
		Waiting int    `json:"waiting"`
		Limit   int    `json:"limit"`
	}
)

// Lease is a lease as the log says it.
type Lease struct {
	LeasePayload
	Holder   Actor
	Claimed  time.Time
	Released bool
}

// Expires is when the lease lapses by itself.
func (l *Lease) Expires() time.Time { return l.Claimed.Add(time.Duration(l.TTL) * time.Second) }

// LiveAt reports whether a lease holds at a time.
func (l *Lease) LiveAt(at time.Time) bool { return !l.Released && at.Before(l.Expires()) }

// LiveLeases are the leases in force at a time, oldest first.
func (s *State) LiveLeases(at time.Time) []*Lease {
	var out []*Lease
	for _, l := range s.Leases {
		if l.LiveAt(at) {
			if t := s.Task(l.Task); t != nil && !s.terminal(t) {
				out = append(out, l)
			}
		}
	}
	return out
}

// TaskOverlaps are the overlaps recorded on a task.
func (s *State) TaskOverlaps(task string) []OverlapPayload {
	var out []OverlapPayload
	for _, o := range s.Overlaps {
		if slices.Contains(o.Tasks, task) {
			out = append(out, o)
		}
	}
	return out
}

// AwaitingReview counts the tasks that wait for the person: reported, at a gate.
func (s *State) AwaitingReview() int {
	n := 0
	for _, t := range s.Tasks {
		if s.terminal(t) {
			continue
		}
		if _, open := t.OpenGate(); open {
			n++
		}
	}
	return n
}

// PathMatch reports whether a changed file (repository/path) falls in a lease's glob.
func PathMatch(glob, file string) bool {
	g := strings.TrimSuffix(strings.TrimSuffix(glob, "*"), "*")
	if g == "" {
		return true
	}
	if strings.HasSuffix(g, "/") {
		return strings.HasPrefix(file, g)
	}
	return file == g || strings.HasPrefix(file, g+"/")
}

func (s *State) foldLease(e Event) error {
	switch e.Type {
	case LeaseClaimed:
		p, ok := decode[LeasePayload](e)
		if !ok || p.Lease == "" || e.Subject != "lease:"+p.Lease {
			return refuse("a lease needs an id")
		}
		t := s.Task(p.Task)
		if t == nil || s.terminal(t) {
			return refuse("a lease belongs to an open task of the mission")
		}
		if p.Path == "" || len(p.Path) > maxPath || strings.HasPrefix(p.Path, "/") || strings.Contains(p.Path, "..") {
			return refuse("a lease names a path relative to the workspace, like api/handler.go or web/")
		}
		if p.Mode != LeaseExclusive && p.Mode != LeaseShared {
			return refuse("a lease is exclusive or shared")
		}
		if p.TTL <= 0 || time.Duration(p.TTL)*time.Second > MaxLeaseTTL {
			return refuse("a lease lasts at most %s", MaxLeaseTTL)
		}
		if len(p.Reason) > maxNote {
			return refuse("reason is longer than %d bytes", maxNote)
		}
		if e.Actor.Kind == ActorAgent && !s.mayLease(e.Actor.Conversation, t) {
			return refuse("an agent leases paths only for its own task, or (an orchestrator) for the tasks of the agents it started")
		}
		for _, l := range s.LiveLeases(e.At) {
			if l.Task == p.Task || (l.Mode != LeaseExclusive && p.Mode != LeaseExclusive) {
				continue
			}
			if _, meet := pathsMeet(l.Path, p.Path); meet {
				owner := s.Task(l.Task)
				return refuse("%s is leased %s by “%s” until %s", l.Path, l.Mode, owner.Title, l.Expires().Format("15:04"))
			}
		}
		s.Leases = append(s.Leases, &Lease{LeasePayload: p, Holder: e.Actor, Claimed: e.At})
	case LeaseReleased:
		p, _ := decode[LeasePayload](e)
		var l *Lease
		for _, x := range s.Leases {
			if x.Lease == p.Lease {
				l = x
			}
		}
		if l == nil || l.Released {
			return refuse("lease %q is not held", p.Lease)
		}
		if e.Actor.Kind == ActorAgent && !s.mayLease(e.Actor.Conversation, s.Task(l.Task)) {
			return refuse("an agent releases only its own task's leases")
		}
		l.Released = true
	case OverlapDetected:
		if e.Actor.Kind != ActorLazybox {
			return refuse("only Lazybox records an overlap it observed")
		}
		p, ok := decode[OverlapPayload](e)
		if !ok || len(p.Tasks) != 2 || len(p.Paths) == 0 || (p.Kind != "files" && p.Kind != "lease") {
			return refuse("an overlap names two tasks and the paths they share")
		}
		s.Overlaps = append(s.Overlaps, p)
	case WIPHeld:
		if e.Actor.Kind != ActorLazybox {
			return refuse("only Lazybox records a hold")
		}
	}
	return nil
}

// mayLease: a task's own attempt, or the orchestrator that started it.
func (s *State) mayLease(conversation string, t *Task) bool {
	if t == nil {
		return false
	}
	if a := t.Current(); a != nil && !a.Ended && (a.Conversation == conversation || a.Parent == conversation) {
		return true
	}
	return false
}

// ---- Operations ---------------------------------------------------------------

// ClaimLease records a lease on a path for a task.
func (s *Store) ClaimLease(mission string, actor Actor, key string, l LeasePayload) (*State, string, error) {
	if l.TTL == 0 {
		l.TTL = int64(DefaultLeaseTTL / time.Second)
	}
	if l.Mode == "" {
		l.Mode = LeaseShared
	}
	l.Path = strings.TrimPrefix(strings.TrimSpace(l.Path), "./")
	l.Lease = shortID("ls")
	state, events, err := s.Append(mission, Draft{Subject: "lease:" + l.Lease, Type: LeaseClaimed, Actor: actor, Idempotency: key, Payload: l})
	if err == nil && len(events) == 0 {
		l.Lease = existingSubject(state, key, "lease:")
	}
	return state, l.Lease, err
}

// ReleaseLease ends a lease.
func (s *Store) ReleaseLease(mission, lease string, actor Actor) (*State, error) {
	state, _, err := s.Append(mission, Draft{Subject: "lease:" + lease, Type: LeaseReleased, Actor: actor, Payload: LeasePayload{Lease: lease}})
	return state, err
}

// RecordOverlap records that two tasks touched the same paths, once per
// pair, kind and set of paths. It reports whether it wrote.
func (s *Store) RecordOverlap(mission string, o OverlapPayload) (*State, bool, error) {
	sort.Strings(o.Tasks)
	sort.Strings(o.Paths)
	key := "overlap:" + strings.Join(o.Tasks, "+") + ":" + o.Kind + ":" + strings.Join(o.Paths, ",")
	if len(key) > maxIdentity {
		key = key[:maxIdentity]
	}
	current, err := s.Mission(mission)
	if err != nil {
		return nil, false, err
	}
	if _, ok := current.Event(key); ok {
		return current, false, nil
	}
	state, events, err := s.Append(mission, Draft{Subject: TaskSubject(o.Tasks[0]), Type: OverlapDetected, Actor: Actor{Kind: ActorLazybox}, Idempotency: key, Payload: o})
	return state, err == nil && len(events) > 0, err
}

// RecordHold records once per grant and waiting count that new agents are held.
func (s *Store) RecordHold(mission string, h HoldPayload) (*State, bool, error) {
	key := "hold:" + h.Grant + ":" + itoa(h.Waiting)
	current, err := s.Mission(mission)
	if err != nil {
		return nil, false, err
	}
	if _, ok := current.Event(key); ok {
		return current, false, nil
	}
	state, events, err := s.Append(mission, Draft{Subject: GrantSubject(h.Grant), Type: WIPHeld, Actor: Actor{Kind: ActorLazybox}, Idempotency: key, Payload: h})
	return state, err == nil && len(events) > 0, err
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var b []byte
	for ; n > 0; n /= 10 {
		b = append([]byte{byte('0' + n%10)}, b...)
	}
	return string(b)
}
