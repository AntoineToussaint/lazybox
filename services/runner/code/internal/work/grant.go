package work

import (
	"slices"
	"strings"
	"time"
)

// The Orchestrator grant (audits/2026-11-16/sota-agents.md §6.3): a person
// lets one conversation run agents for one mission. With it the conversation
// may start conversations (each an attempt on a task of the mission, with
// the holder as its parent) and drive only those: message, wait on, read and
// stop them. Nothing a grant holds approves a request, answers a question
// for the person, passes a gate or accepts a task: those stay owner RPCs, and
// the log refuses a gate decided by anyone but a person (fold.go).
//
// Capabilities only shrink down the tree. A child's permission is at most the
// grant's ceiling (itself at most what the person chose), it runs only on a
// subscription the grant names, and a grant issued by an agent to its child
// (depth > 1) is a subset of the issuer's in every field. The log enforces
// all of it when it folds, so a replayed log and a running Runner agree.

const (
	GrantIssued  = "grant.issued"
	GrantRevoked = "grant.revoked"
	MessageSent  = "message.sent"
)

// Scopes a grant holds. They are named in the log and on screen; the Runner
// checks the grant itself, never a scope string an agent presents.
const (
	ScopeSpawn      = "session.spawn"
	ScopeSend       = "session.send"
	ScopeWait       = "session.wait"
	ScopeStop       = "session.stop"
	ScopeTaskCreate = "task.create"
)

// GrantScopes is what an Orchestrator grant holds. There is deliberately no
// scope that approves, answers, passes a gate or accepts a task.
var GrantScopes = []string{ScopeSpawn, ScopeSend, ScopeWait, ScopeStop, ScopeTaskCreate}

// Permission levels a child may run at, narrowest first. "inherit" (the
// provider's own default) has no known level, so a child never gets it and a
// holder running with it counts as ask.
const (
	PermissionReadOnly = "read-only"
	PermissionAsk      = "ask"
	PermissionYolo     = "yolo"
)

var permissionRank = map[string]int{PermissionReadOnly: 0, PermissionAsk: 1, PermissionYolo: 2}

// PermissionRank orders permission levels; ok is false for an unknown one.
func PermissionRank(p string) (int, bool) { r, ok := permissionRank[p]; return r, ok }

// EffectivePermission is the level a conversation runs at, for comparing:
// "inherit" and "" (the provider's default) count as ask.
func EffectivePermission(p string) string {
	if _, ok := permissionRank[p]; ok {
		return p
	}
	return PermissionAsk
}

// NarrowerPermission is the narrower of two levels.
func NarrowerPermission(a, b string) string {
	a, b = EffectivePermission(a), EffectivePermission(b)
	if permissionRank[a] <= permissionRank[b] {
		return a
	}
	return b
}

// Bounds of a grant.
const (
	DefaultMaxChildren = 3
	MaxChildrenLimit   = 16
	MaxDepth           = 3
	maxProfiles        = 16
	// MaxMessage bounds one message between agents: references, not pasted output.
	MaxMessage = 4 << 10
	// MessageWindow and MessagesPerWindow bound messages per direction of a
	// parent–child pair.
	MessageWindow     = time.Minute
	MessagesPerWindow = 6
)

// Ceiling is the most a grant's children may get.
type Ceiling struct {
	// Permission is read-only, ask or yolo.
	Permission string `json:"permission"`
	// Profiles are the subscriptions children may run on; never empty.
	Profiles []string `json:"profiles"`
	// MaxChildren bounds the children live at once.
	MaxChildren int `json:"max_children"`
	// Depth 1: children start no agents. Each level down is one less.
	Depth int `json:"depth"`
	// BudgetUSD is what the whole tree may spend; 0 sets no cap.
	BudgetUSD float64 `json:"budget_usd,omitempty"`
	// PlanRequired: no agent starts before the person approved a plan
	// (plan.go); then only the approved plan's tasks start.
	PlanRequired bool `json:"plan_required,omitempty"`
	// ReviewLimit holds new agents while this many tasks wait for the
	// person's review (lease.go); 0 is DefaultReviewLimit.
	ReviewLimit int `json:"review_limit,omitempty"`
}

func (c Ceiling) reviewLimit() int {
	if c.ReviewLimit <= 0 {
		return DefaultReviewLimit
	}
	return c.ReviewLimit
}

// Within reports whether c asks for no more than limit, and says what is wider.
func (c Ceiling) Within(limit Ceiling) (bool, string) {
	if permissionRank[c.Permission] > permissionRank[limit.Permission] {
		return false, "a wider permission (" + c.Permission + ") than " + limit.Permission
	}
	for _, p := range c.Profiles {
		if !slices.Contains(limit.Profiles, p) {
			return false, "subscription " + p + ", which it may not use"
		}
	}
	if c.MaxChildren > limit.MaxChildren {
		return false, "more children than its own limit"
	}
	if c.Depth >= limit.Depth {
		return false, "as much depth as itself"
	}
	if limit.BudgetUSD > 0 && (c.BudgetUSD <= 0 || c.BudgetUSD > limit.BudgetUSD) {
		return false, "more budget than its own"
	}
	if limit.PlanRequired && !c.PlanRequired {
		return false, "the right to start agents without an approved plan"
	}
	if c.reviewLimit() > limit.reviewLimit() {
		return false, "more work waiting for review than its own"
	}
	return true, ""
}

func (c Ceiling) validate() error {
	if _, ok := permissionRank[c.Permission]; !ok {
		return refuse("a grant's permission ceiling is read-only, ask or yolo")
	}
	if len(c.Profiles) == 0 || len(c.Profiles) > maxProfiles {
		return refuse("a grant names between 1 and %d subscriptions its children may use", maxProfiles)
	}
	seen := map[string]bool{}
	for _, p := range c.Profiles {
		if p == "" || len(p) > maxIdentity || seen[p] {
			return refuse("a grant's subscriptions must be distinct ids")
		}
		seen[p] = true
	}
	if c.MaxChildren < 1 || c.MaxChildren > MaxChildrenLimit {
		return refuse("a grant allows between 1 and %d children at once", MaxChildrenLimit)
	}
	if c.Depth < 1 || c.Depth > MaxDepth {
		return refuse("a grant's depth is between 1 and %d", MaxDepth)
	}
	if c.BudgetUSD < 0 || c.BudgetUSD > 10000 {
		return refuse("a grant's budget is between $0 (no cap) and $10,000")
	}
	if c.ReviewLimit < 0 || c.ReviewLimit > MaxChildrenLimit {
		return refuse("a grant holds new agents at between 1 and %d tasks waiting for review", MaxChildrenLimit)
	}
	return nil
}

type (
	// GrantPayload is grant.issued.
	GrantPayload struct {
		Grant string `json:"grant"`
		// Holder is the conversation that may orchestrate; Session the
		// execution it ran in when it was granted.
		Holder  string `json:"holder"`
		Session string `json:"session,omitempty"`
		Title   string `json:"title,omitempty"`
		// Parent is the grant of the agent that narrowed this one for its child.
		Parent  string   `json:"parent,omitempty"`
		Scopes  []string `json:"scopes"`
		Ceiling Ceiling  `json:"ceiling"`
	}
	// RevokePayload is grant.revoked.
	RevokePayload struct {
		Grant  string `json:"grant"`
		Reason string `json:"reason,omitempty"`
	}
	// MessagePayload is message.sent: one message between a parent and its child.
	MessagePayload struct {
		Message   string `json:"message"`
		From      string `json:"from"`
		To        string `json:"to"`
		FromLabel string `json:"from_label,omitempty"`
		ToLabel   string `json:"to_label,omitempty"`
		// Kind is note (a message) or report (a child's report, as told to its parent).
		Kind string `json:"kind"`
		Text string `json:"text"`
	}
)

// Grant is a grant as the log says it.
type Grant struct {
	GrantPayload
	IssuedBy  Actor
	Issued    time.Time
	IssuedSeq uint64
	Revoked   bool
	RevokedAt time.Time
	Reason    string
	RevokedBy Actor
}

// Message is one message.sent.
type Message struct {
	MessagePayload
	Seq uint64
	At  time.Time
}

// GrantSubject and MessageSubject name a grant's and a message's events.
func GrantSubject(id string) string   { return "grant:" + id }
func MessageSubject(id string) string { return "message:" + id }

// Grant returns a grant of the mission, or nil.
func (s *State) Grant(id string) *Grant {
	for _, g := range s.Grants {
		if g.Grant == id {
			return g
		}
	}
	return nil
}

// Live reports whether a grant is in force: neither it nor any grant it was
// narrowed from is revoked, and the mission goes on.
func (s *State) Live(g *Grant) bool {
	for depth := 0; g != nil && depth <= MaxDepth+1; depth++ {
		if g.Revoked || s.Canceled {
			return false
		}
		if g.Parent == "" {
			return true
		}
		g = s.Grant(g.Parent)
	}
	return false
}

// HeldBy is the live grant a conversation holds in this mission, or nil.
func (s *State) HeldBy(conversation string) *Grant {
	for _, g := range s.Grants {
		if g.Holder == conversation && s.Live(g) {
			return g
		}
	}
	return nil
}

// Child is one conversation a grant's holder started, as its task's latest
// attempt there says.
type Child struct {
	Task    *Task
	Attempt *Attempt
}

// Children lists the conversations a holder started under any of its grants
// in this mission, one row per child conversation (its latest attempt).
func (s *State) Children(holder string) []Child {
	var out []Child
	index := map[string]int{}
	for _, t := range s.Tasks {
		for _, a := range t.Attempts {
			if a.Parent != holder || a.Parent == "" {
				continue
			}
			if i, ok := index[a.Conversation]; ok {
				out[i] = Child{Task: t, Attempt: a}
				continue
			}
			index[a.Conversation] = len(out)
			out = append(out, Child{Task: t, Attempt: a})
		}
	}
	return out
}

// LiveChildren counts a grant's children whose attempt has not ended on a
// task that is still open.
func (s *State) LiveChildren(grant string) int {
	n := 0
	for _, t := range s.Tasks {
		if s.terminal(t) {
			continue
		}
		if a := t.Current(); a != nil && !a.Ended && a.Grant == grant {
			n++
		}
	}
	return n
}

// ParentOf is the attempt that started a conversation as a child, or nil.
func (s *State) ParentOf(conversation string) (*Task, *Attempt) {
	var task *Task
	var found *Attempt
	for _, t := range s.Tasks {
		for _, a := range t.Attempts {
			if a.Conversation == conversation && a.Parent != "" {
				task, found = t, a
			}
		}
	}
	return task, found
}

// Related reports whether two conversations are a parent and its child in
// this mission: the only pairs that may message each other.
func (s *State) Related(a, b string) bool {
	if a == "" || b == "" || a == b {
		return false
	}
	for _, t := range s.Tasks {
		for _, at := range t.Attempts {
			if (at.Parent == a && at.Conversation == b) || (at.Parent == b && at.Conversation == a) {
				return true
			}
		}
	}
	return false
}

// SentSince counts the messages from one conversation to another since a time.
func (s *State) SentSince(from, to string, since time.Time) int {
	n := 0
	for i := len(s.Messages) - 1; i >= 0; i-- {
		m := s.Messages[i]
		if m.At.Before(since) {
			break
		}
		if m.From == from && m.To == to {
			n++
		}
	}
	return n
}

// foldGrant folds the grant and message families.
func (s *State) foldGrant(e Event) error {
	switch e.Type {
	case GrantIssued:
		p, ok := decode[GrantPayload](e)
		if !ok || p.Grant == "" || p.Holder == "" {
			return refuse("a grant needs an id and the conversation that holds it")
		}
		if e.Subject != GrantSubject(p.Grant) {
			return refuse("subject %q does not name grant %q", e.Subject, p.Grant)
		}
		if s.Grant(p.Grant) != nil {
			return refuse("grant %q exists", p.Grant)
		}
		if err := p.Ceiling.validate(); err != nil {
			return err
		}
		for _, scope := range p.Scopes {
			if !slices.Contains(GrantScopes, scope) {
				return refuse("a grant cannot hold %q", scope)
			}
		}
		if held := s.HeldBy(p.Holder); held != nil {
			return refuse("that conversation already orchestrates this mission")
		}
		if p.Parent == "" {
			if e.Actor.Kind != ActorHuman {
				// Only a person gives an agent the power to run agents.
				return refuse("only a person grants a conversation the right to run agents")
			}
		} else {
			parent := s.Grant(p.Parent)
			if parent == nil || !s.Live(parent) {
				return refuse("grant %q is not in force", p.Parent)
			}
			if e.Actor.Kind != ActorAgent || e.Actor.Conversation != parent.Holder {
				return refuse("only the holder of a grant narrows it for its child")
			}
			if _, a := s.ParentOf(p.Holder); a == nil || a.Parent != parent.Holder {
				return refuse("a grant is narrowed only for the holder's own child")
			}
			if ok, why := p.Ceiling.Within(parent.Ceiling); !ok {
				return refuse("a child cannot get %s", why)
			}
			for _, scope := range p.Scopes {
				if !slices.Contains(parent.Scopes, scope) {
					return refuse("a child cannot get %q, which its parent does not hold", scope)
				}
			}
		}
		s.Grants = append(s.Grants, &Grant{GrantPayload: p, IssuedBy: e.Actor, Issued: e.At, IssuedSeq: e.Seq})
	case GrantRevoked:
		p, ok := decode[RevokePayload](e)
		g := s.Grant(p.Grant)
		if !ok || g == nil {
			return refuse("grant %q does not exist", p.Grant)
		}
		if g.Revoked {
			return refuse("grant %q is already revoked", p.Grant)
		}
		if e.Actor.Kind == ActorAgent {
			// An agent revokes only a grant it narrowed for its own child.
			parent := s.Grant(g.Parent)
			if parent == nil || parent.Holder != e.Actor.Conversation {
				return refuse("only the person, or the agent that narrowed it, revokes a grant")
			}
		} else if e.Actor.Kind != ActorHuman && e.Actor.Kind != ActorLazybox {
			return refuse("only the person revokes a grant")
		}
		if len(p.Reason) > maxNote {
			return refuse("reason is longer than %d bytes", maxNote)
		}
		g.Revoked, g.RevokedAt, g.Reason, g.RevokedBy = true, e.At, p.Reason, e.Actor
	case MessageSent:
		p, ok := decode[MessagePayload](e)
		if !ok || p.Message == "" || p.From == "" || p.To == "" {
			return refuse("a message needs an id, a sender and a receiver")
		}
		if e.Subject != MessageSubject(p.Message) {
			return refuse("subject %q does not name message %q", e.Subject, p.Message)
		}
		if e.Actor.Kind != ActorAgent || e.Actor.Conversation != p.From {
			// A person's message is the person's, in the conversation; never an agent's.
			return refuse("a message between agents is sent by the sending agent")
		}
		if !s.Related(p.From, p.To) {
			return refuse("agents message only their parent or the children they started")
		}
		if p.Kind != "note" && p.Kind != "report" {
			return refuse("a message is a note or a report")
		}
		if strings.TrimSpace(p.Text) == "" || len(p.Text) > MaxMessage {
			return refuse("a message holds 1 to %d bytes; send references (a file, a run, an artifact) rather than pasted output", MaxMessage)
		}
		if s.SentSince(p.From, p.To, e.At.Add(-MessageWindow)) >= MessagesPerWindow {
			return refuse("at most %d messages a minute to the same agent; wait for its reply", MessagesPerWindow)
		}
		s.Messages = append(s.Messages, &Message{MessagePayload: p, Seq: e.Seq, At: e.At})
	}
	return nil
}

// checkLineage is what an assignment that starts a child must satisfy: the
// grant is in force, its holder is the parent, there is room for one more
// live child, and the child runs within the ceiling.
func (s *State) checkLineage(e Event, p AttemptPayload) error {
	if p.Grant == "" && p.Parent == "" {
		return nil
	}
	g := s.Grant(p.Grant)
	if g == nil || !s.Live(g) {
		return refuse("the orchestrator grant is not in force")
	}
	if p.Parent != g.Holder || e.Actor.Kind != ActorAgent || e.Actor.Conversation != g.Holder {
		return refuse("only the grant's holder starts its children")
	}
	if p.Conversation == g.Holder {
		return refuse("an orchestrator cannot be its own child")
	}
	if s.LiveChildren(g.Grant) >= g.Ceiling.MaxChildren {
		return refuse("the grant allows %d agents at once; wait for one to finish or stop one", g.Ceiling.MaxChildren)
	}
	if rank, ok := permissionRank[p.Permission]; !ok || rank > permissionRank[g.Ceiling.Permission] {
		return refuse("a child runs at %s at most", g.Ceiling.Permission)
	}
	if !slices.Contains(g.Ceiling.Profiles, p.Profile) {
		return refuse("the grant does not let children use subscription %q", p.Profile)
	}
	return nil
}
