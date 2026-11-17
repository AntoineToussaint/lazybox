package work

import (
	"errors"
	"strings"
)

// Operations on Orchestrator grants, the children they start and the
// messages between them (grant.go). Each is a batch of drafts the log
// validates, like ops.go.

// Held is a live grant and the mission it is in.
type Held struct {
	Mission *State
	Grant   *Grant
}

// HeldBy is the live grant a conversation holds in any mission.
func (s *Store) HeldBy(conversation string) (Held, bool) {
	if conversation == "" {
		return Held{}, false
	}
	for _, m := range s.Missions() {
		if m.Err != nil {
			continue
		}
		if g := m.State.HeldBy(conversation); g != nil {
			return Held{Mission: m.State, Grant: g}, true
		}
	}
	return Held{}, false
}

// Lineage is where a child conversation came from.
type Lineage struct {
	Mission *State
	Task    *Task
	Attempt *Attempt
}

// ParentOf is the attempt that started a conversation as a child, in the
// mission that changed most recently (a conversation is normally the child
// of one orchestrator).
func (s *Store) ParentOf(conversation string) (Lineage, bool) {
	if conversation == "" {
		return Lineage{}, false
	}
	// Missions lists the most recently changed first.
	for _, m := range s.Missions() {
		if m.Err != nil {
			continue
		}
		if t, a := m.State.ParentOf(conversation); a != nil {
			return Lineage{Mission: m.State, Task: t, Attempt: a}, true
		}
	}
	return Lineage{}, false
}

// IssueGrant lets a conversation run agents for a mission. Only a person
// issues a root grant (the fold refuses anyone else); a conversation
// orchestrates one mission at a time.
func (s *Store) IssueGrant(mission string, actor Actor, key string, g GrantPayload) (*State, string, error) {
	if g.Holder == "" {
		return nil, "", refuse("choose the conversation that orchestrates")
	}
	if key != "" {
		if current, err := s.Mission(mission); err == nil {
			if e, ok := current.Event(key); ok {
				return current, strings.TrimPrefix(e.Subject, "grant:"), nil
			}
		}
	}
	if held, ok := s.HeldBy(g.Holder); ok && held.Mission.ID != mission {
		return nil, "", refuse("that conversation already orchestrates “%s”; revoke that first", held.Mission.Title)
	}
	title, err := cleanText(g.Title, maxTitle, "title")
	if err != nil {
		return nil, "", err
	}
	g.Title = title
	if len(g.Scopes) == 0 {
		g.Scopes = append([]string(nil), GrantScopes...)
	}
	g.Grant = shortID("gr")
	state, events, err := s.Append(mission, Draft{Subject: GrantSubject(g.Grant), Type: GrantIssued, Actor: actor, Idempotency: key, Payload: g})
	if err == nil && len(events) == 0 {
		g.Grant = existingSubject(state, key, "grant:")
	}
	return state, g.Grant, err
}

// RevokeGrant ends a grant and every grant narrowed from it. Its children
// keep running: stopping them is the person's choice.
func (s *Store) RevokeGrant(mission, grant string, actor Actor, key, reason string) (*State, error) {
	reason, err := cleanText(reason, maxNote, "reason")
	if err != nil {
		return nil, err
	}
	state, _, err := s.Append(mission, Draft{Subject: GrantSubject(grant), Type: GrantRevoked, Actor: actor, Idempotency: key, Payload: RevokePayload{Grant: grant, Reason: reason}})
	return state, err
}

// RevokeHeld revokes every live grant a conversation holds (it was deleted).
func (s *Store) RevokeHeld(conversation, reason string) error {
	for range MaxDepth + 8 {
		held, ok := s.HeldBy(conversation)
		if !ok {
			return nil
		}
		if _, err := s.RevokeGrant(held.Mission.ID, held.Grant.Grant, Actor{Kind: ActorLazybox}, "", reason); err != nil {
			return err
		}
	}
	return errors.New("grants kept reappearing")
}

// Spawn records a child an orchestrator started: a new task (unless it works
// on an existing one) and its assignment to the child's conversation, with
// the lineage. The log checks the grant, the room for one more child and the
// ceiling (checkLineage); a refused spawn writes nothing.
func (s *Store) Spawn(mission string, actor Actor, key, task, title, brief string, attempt AttemptPayload) (*State, string, string, error) {
	current, err := s.Mission(mission)
	if err != nil {
		return nil, "", "", err
	}
	if key != "" {
		if e, ok := current.Event(key); ok {
			p, _ := decode[AttemptPayload](e)
			return current, p.Task, p.Attempt, nil
		}
	}
	var drafts []Draft
	if task == "" {
		if title, err = cleanText(title, maxTitle, "title"); err != nil {
			return nil, "", "", err
		}
		if brief, err = cleanText(brief, maxBrief, "brief"); err != nil {
			return nil, "", "", err
		}
		if title == "" {
			return nil, "", "", refuse("a child's task needs a title")
		}
		task = shortID("t")
		drafts = append(drafts, Draft{Subject: TaskSubject(task), Type: TaskCreated, Actor: actor, Payload: TaskPayload{Task: task, Title: title, Brief: brief}})
	} else if t := current.Task(task); t == nil {
		return nil, "", "", refuse("task %q does not exist", task)
	} else if a := t.Current(); a != nil && !a.Ended {
		return nil, "", "", refuse("“%s” already has a live conversation", t.Title)
	}
	attempt.Task, attempt.Attempt, attempt.New = task, shortID("a"), true
	drafts = append(drafts,
		Draft{Subject: TaskSubject(task), Type: TaskAssigned, Actor: actor, Idempotency: key, Payload: attempt},
		Draft{Subject: TaskSubject(task), Type: AttemptStarted, Actor: Actor{Kind: ActorLazybox, Conversation: attempt.Conversation, Execution: attempt.Execution}, Payload: AttemptPayload{Task: task, Attempt: attempt.Attempt, Conversation: attempt.Conversation, Workspace: attempt.Workspace, Execution: attempt.Execution}},
	)
	state, _, err := s.Append(mission, drafts...)
	return state, task, attempt.Attempt, err
}

// StopChild ends a child's attempt, as its parent asked.
func (s *Store) StopChild(mission, task, attempt string, actor Actor, conversation, outcome string) (*State, error) {
	state, _, err := s.Append(mission, Draft{Subject: TaskSubject(task), Type: AttemptEnded, Actor: actor, Payload: AttemptPayload{Task: task, Attempt: attempt, Conversation: conversation, Outcome: outcome}})
	return state, err
}

// Send records a message between a parent and its child. The log checks the
// pair, the size and the rate.
func (s *Store) Send(mission string, actor Actor, key string, m MessagePayload) (*State, string, error) {
	m.Message = shortID("msg")
	if m.Kind == "" {
		m.Kind = "note"
	}
	m.Text = strings.TrimSpace(m.Text)
	state, events, err := s.Append(mission, Draft{Subject: MessageSubject(m.Message), Type: MessageSent, Actor: actor, Idempotency: key, Payload: m})
	if err == nil && len(events) == 0 {
		m.Message = existingSubject(state, key, "message:")
	}
	return state, m.Message, err
}
