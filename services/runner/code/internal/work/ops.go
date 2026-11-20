package work

import "errors"

// The operations a person (through the owner's RPCs) and the Runner (through
// its evidence hooks) perform on missions. Each is a batch of drafts the log
// validates; none writes state outside the log.

// Human is the person at the Runner: every owner RPC acts as them.
var Human = Actor{Kind: ActorHuman}

// EditMission changes the title or brief.
func (s *Store) EditMission(id string, actor Actor, key, title, brief string) (*State, error) {
	title, err := cleanText(title, maxTitle, "title")
	if err != nil {
		return nil, err
	}
	if brief, err = cleanText(brief, maxBrief, "brief"); err != nil {
		return nil, err
	}
	state, _, err := s.Append(id, Draft{Subject: MissionSubject, Type: MissionBriefChanged, Actor: actor, Idempotency: key, Payload: MissionPayload{Title: title, Brief: brief}})
	return state, err
}

// CancelMission ends a mission; its tasks read as canceled.
func (s *Store) CancelMission(id string, actor Actor, key, reason string) (*State, error) {
	reason, err := cleanText(reason, maxNote, "reason")
	if err != nil {
		return nil, err
	}
	state, _, err := s.Append(id, Draft{Subject: MissionSubject, Type: MissionCanceled, Actor: actor, Idempotency: key, Payload: CancelPayload{Reason: reason}})
	return state, err
}

// AddGoal adds a goal: a checkpoint tasks are grouped under.
func (s *Store) AddGoal(id string, actor Actor, key, title string) (*State, string, error) {
	title, err := cleanText(title, maxTitle, "title")
	if err != nil {
		return nil, "", err
	}
	if title == "" {
		return nil, "", refuse("a goal needs a title")
	}
	goal := shortID("g")
	state, events, err := s.Append(id, Draft{Subject: GoalSubject(goal), Type: GoalCreated, Actor: actor, Idempotency: key, Payload: GoalPayload{Goal: goal, Title: title}})
	if err == nil && len(events) == 0 {
		goal = existingSubject(state, key, "goal:")
	}
	return state, goal, err
}

// AddTask adds a task: one assignment with a brief (objective, done
// criteria, boundaries, output shape).
func (s *Store) AddTask(id string, actor Actor, key, goal, title, brief string, blockedBy []string) (*State, string, error) {
	title, err := cleanText(title, maxTitle, "title")
	if err != nil {
		return nil, "", err
	}
	if brief, err = cleanText(brief, maxBrief, "brief"); err != nil {
		return nil, "", err
	}
	if title == "" {
		return nil, "", refuse("a task needs a title")
	}
	task := shortID("t")
	state, events, err := s.Append(id, Draft{Subject: TaskSubject(task), Type: TaskCreated, Actor: actor, Idempotency: key, Payload: TaskPayload{Task: task, Goal: goal, Title: title, Brief: brief, BlockedBy: blockedBy}})
	if err == nil && len(events) == 0 {
		task = existingSubject(state, key, "task:")
	}
	return state, task, err
}

func existingSubject(state *State, key, prefix string) string {
	if e, ok := state.Event(key); ok && len(e.Subject) > len(prefix) {
		return e.Subject[len(prefix):]
	}
	return ""
}

// EditTask changes a task's title or brief.
func (s *Store) EditTask(id, task string, actor Actor, key, title, brief string) (*State, error) {
	title, err := cleanText(title, maxTitle, "title")
	if err != nil {
		return nil, err
	}
	if brief, err = cleanText(brief, maxBrief, "brief"); err != nil {
		return nil, err
	}
	state, _, err := s.Append(id, Draft{Subject: TaskSubject(task), Type: TaskBriefChanged, Actor: actor, Idempotency: key, Payload: TaskPayload{Task: task, Title: title, Brief: brief}})
	return state, err
}

// CancelTask cancels a task.
func (s *Store) CancelTask(id, task string, actor Actor, key, reason string) (*State, error) {
	reason, err := cleanText(reason, maxNote, "reason")
	if err != nil {
		return nil, err
	}
	state, _, err := s.Append(id, Draft{Subject: TaskSubject(task), Type: TaskCanceled, Actor: actor, Idempotency: key, Payload: CancelPayload{Task: task, Reason: reason}})
	return state, err
}

// Assign gives a task to a conversation, as a new attempt. A live attempt on
// another conversation is ended as a hand-off first, in the same batch.
func (s *Store) Assign(id, task string, actor Actor, key string, attempt AttemptPayload) (*State, string, error) {
	if attempt.Conversation == "" {
		return nil, "", refuse("choose a conversation to assign the task to")
	}
	current, err := s.Mission(id)
	if err != nil {
		return nil, "", err
	}
	if key != "" {
		if e, ok := current.Event(key); ok {
			p, _ := decode[AttemptPayload](e)
			return current, p.Attempt, nil
		}
	}
	attempt.Task = task
	attempt.Attempt = shortID("a")
	var drafts []Draft
	if t := current.Task(task); t != nil {
		if a := t.Current(); a != nil && !a.Ended {
			if a.Conversation == attempt.Conversation {
				return current, a.ID, nil
			}
			drafts = append(drafts, Draft{Subject: TaskSubject(task), Type: AttemptEnded, Actor: actor, Payload: AttemptPayload{Task: task, Attempt: a.ID, Conversation: a.Conversation, Outcome: "handoff"}})
		}
	}
	drafts = append(drafts,
		Draft{Subject: TaskSubject(task), Type: TaskAssigned, Actor: actor, Idempotency: key, Payload: attempt},
		Draft{Subject: TaskSubject(task), Type: AttemptStarted, Actor: Actor{Kind: ActorLazybox, Conversation: attempt.Conversation, Execution: attempt.Execution}, Payload: AttemptPayload{Task: task, Attempt: attempt.Attempt, Conversation: attempt.Conversation, Workspace: attempt.Workspace, Execution: attempt.Execution}},
	)
	state, _, err := s.Append(id, drafts...)
	return state, attempt.Attempt, err
}

// Report claims a task done. The claim cites the evidence its current attempt
// recorded; whether that evidence passes is derived, not claimed.
func (s *Store) Report(id, task string, actor Actor, key, summary string) (*State, error) {
	summary, err := cleanText(summary, maxSummary, "summary")
	if err != nil {
		return nil, err
	}
	current, err := s.Mission(id)
	if err != nil {
		return nil, err
	}
	var refs []string
	if t := current.Task(task); t != nil {
		start := uint64(0)
		if a := t.Current(); a != nil {
			start = a.StartSeq
		}
		seen := map[string]bool{}
		for _, r := range t.Evidence {
			if r.Seq >= start && !seen[r.Ref] {
				seen[r.Ref] = true
				refs = append(refs, r.Ref)
			}
		}
	}
	state, _, err := s.Append(id, Draft{Subject: TaskSubject(task), Type: TaskReported, Actor: actor, Idempotency: key, Evidence: refs, Payload: ReportPayload{Task: task, Summary: summary}})
	return state, err
}

// Decide passes or rejects the gate open on a task. Only a person decides.
func (s *Store) Decide(id, task string, actor Actor, key string, pass bool, note string) (*State, error) {
	note, err := cleanText(note, maxNote, "note")
	if err != nil {
		return nil, err
	}
	current, err := s.Mission(id)
	if err != nil {
		return nil, err
	}
	if key != "" {
		if _, ok := current.Event(key); ok {
			return current, nil
		}
	}
	t := current.Task(task)
	if t == nil {
		return nil, refuse("task %q does not exist", task)
	}
	gate, open := t.OpenGate()
	if !open {
		return nil, refuse("nothing on “%s” is waiting for a decision", t.Title)
	}
	kind := GateRejected
	if pass {
		kind = GatePassed
	}
	state, _, err := s.Append(id, Draft{Subject: TaskSubject(task), Type: kind, Actor: actor, Idempotency: key, CausedBy: eventID(current, gate.Seq), Payload: GatePayload{Gate: gate.Gate, Task: task, Note: note}})
	return state, err
}

func eventID(s *State, seq uint64) string {
	if seq >= 1 && int(seq) <= len(s.Events) && s.Events[seq-1].Seq == seq {
		return s.Events[seq-1].ID
	}
	return ""
}

// Where evidence came from, to find the attempts it belongs to.
type Origin struct {
	// Conversation: an agent's own run belongs to that conversation's attempts.
	Conversation string
	// Workspace: a terminal run, a commit or a pull request belongs to every
	// attempt in that workspace.
	Workspace string
}

func (o Origin) matches(a Attempt) bool {
	if o.Conversation != "" {
		return a.Conversation == o.Conversation
	}
	return o.Workspace != "" && a.Workspace == o.Workspace
}

// Record attaches evidence to every live attempt it belongs to, at most once
// per reference and state: the same pull request with the same head and
// checks is not recorded twice. It returns how many tasks it was recorded on.
func (s *Store) Record(origin Origin, actor Actor, ev Evidence) (int, error) {
	if ev.Ref == "" {
		return 0, refuse("evidence needs a reference")
	}
	recorded := 0
	var errs []error
	for _, at := range s.Attempts() {
		if !origin.matches(at.Attempt) {
			continue
		}
		state, err := s.Mission(at.Mission)
		if err != nil {
			continue
		}
		if t := state.Task(at.Task); t != nil {
			if last, ok := t.LastEvidence(ev.Ref); ok && last.Seq >= at.Attempt.StartSeq && sameEvidence(last.Evidence, ev) {
				continue
			}
		}
		item := ev
		item.Task, item.Attempt = at.Task, at.Attempt.ID
		if _, _, err := s.Append(at.Mission, Draft{Subject: TaskSubject(at.Task), Type: EvidenceRecorded, Actor: actor, Evidence: []string{ev.Ref}, Payload: item}); err != nil {
			errs = append(errs, err)
			continue
		}
		recorded++
	}
	return recorded, errors.Join(errs...)
}

func sameEvidence(a, b Evidence) bool {
	return a.Outcome == b.Outcome && a.Head == b.Head && a.State == b.State && a.Detail == b.Detail && a.Blockers == b.Blockers && a.Findings == b.Findings
}

// EndAttempts ends every live attempt on a conversation (it was deleted).
func (s *Store) EndAttempts(conversation, outcome string) error {
	var errs []error
	for _, at := range s.Attempts() {
		if at.Attempt.Conversation != conversation {
			continue
		}
		_, _, err := s.Append(at.Mission, Draft{Subject: TaskSubject(at.Task), Type: AttemptEnded, Actor: Actor{Kind: ActorLazybox}, Payload: AttemptPayload{Task: at.Task, Attempt: at.Attempt.ID, Conversation: conversation, Outcome: outcome}})
		errs = append(errs, err)
	}
	return errors.Join(errs...)
}

// InvalidateReview retracts historical passes on their original tasks and
// attempts. Corrections are journaled but never become a newer review verdict.
func (s *Store) InvalidateReview(ref, detail string) error {
	if ref == "" {
		return refuse("review correction needs a reference")
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	var errs []error
	for id, m := range s.missions {
		if m.err != nil {
			errs = append(errs, m.err)
			continue
		}
		var drafts []Draft
		for _, task := range m.state.Tasks {
			for _, recorded := range task.Evidence {
				if recorded.Kind != EvidenceReview || recorded.Ref != ref || recorded.Outcome != OutcomePass {
					continue
				}
				evidence := recorded.Evidence
				evidence.Corrects, evidence.Outcome, evidence.Detail = recorded.Seq, OutcomeUnknown, detail
				drafts = append(drafts, Draft{Subject: TaskSubject(task.ID), Type: EvidenceRecorded, Actor: Actor{Kind: ActorLazybox}, Payload: evidence})
			}
		}
		if len(drafts) > 0 {
			_, err := s.appendLocked(m, id, drafts)
			if err != nil {
				errs = append(errs, err)
			}
		}
	}
	return errors.Join(errs...)
}
