package work

import (
	"time"
)

// State is the fold of one mission's log: what the events say, before live
// observations. It is rebuilt from the log, never stored, and never mutated
// after Fold returns it.
type State struct {
	ID       string
	Title    string
	Brief    string
	Policy   Policy
	Process  ProcessPayload
	Created  time.Time
	Updated  time.Time
	Canceled bool
	Reason   string
	Seq      uint64
	Goals    []*Goal
	Tasks    []*Task
	// Grants and Messages: the Orchestrator grants and the messages between
	// agents (grant.go).
	Grants   []*Grant
	Messages []*Message
	// Plans are orchestrators' proposals and the person's decisions (plan.go).
	Plans []*Plan
	// Leases and Overlaps: advisory path leases and the overlaps the Runner
	// observed between tasks (lease.go).
	Leases   []*Lease
	Overlaps []OverlapPayload
	Events   []Event
	// Ignored counts events kept in the log but not folded: unknown types,
	// newer versions, or (when replaying leniently) events this Runner would refuse.
	Ignored int

	tasks map[string]*Task
	goals map[string]*Goal
	idem  map[string]int
}

type Goal struct {
	ID      string
	Title   string
	Created time.Time
}

type Task struct {
	ID           string
	Goal         string
	Title        string
	Brief        string
	BlockedBy    []string
	MergeAfter   []string
	Created      time.Time
	Updated      time.Time
	Canceled     bool
	CancelReason string
	Attempts     []*Attempt
	Reports      []Report
	Evidence     []Recorded
	Gates        []GateEvent
}

type Attempt struct {
	ID           string
	Task         string
	Conversation string
	Workspace    string
	Execution    string
	New          bool
	// Lineage (grant.go): set when an orchestrator started this conversation.
	Parent, Grant, Role, Permission, Profile string
	Started                                  time.Time
	StartSeq                                 uint64
	Ended                                    bool
	EndedAt                                  time.Time
	Outcome                                  string
}

type Report struct {
	Seq      uint64
	At       time.Time
	Summary  string
	Actor    Actor
	Evidence []string
}

type Recorded struct {
	Seq   uint64
	At    time.Time
	Actor Actor
	Evidence
}

type GateEvent struct {
	Seq   uint64
	At    time.Time
	Type  string // gate.requested, gate.passed or gate.rejected
	Gate  string
	Note  string
	Actor Actor
}

// Task returns a task of the mission, or nil.
func (s *State) Task(id string) *Task { return s.tasks[id] }

// Event returns the event recorded under an idempotency key.
func (s *State) Event(idempotency string) (Event, bool) {
	if i, ok := s.idem[idempotency]; ok {
		return s.Events[i], true
	}
	return Event{}, false
}

// Current is the task's latest attempt, or nil.
func (t *Task) Current() *Attempt {
	if len(t.Attempts) == 0 {
		return nil
	}
	return t.Attempts[len(t.Attempts)-1]
}

// OpenGate is the gate requested on the task and not yet passed or rejected.
func (t *Task) OpenGate() (GateEvent, bool) {
	open := map[string]GateEvent{}
	order := []string{}
	for _, g := range t.Gates {
		switch g.Type {
		case GateRequested:
			if _, ok := open[g.Gate]; !ok {
				order = append(order, g.Gate)
			}
			open[g.Gate] = g
		default:
			delete(open, g.Gate)
		}
	}
	for _, id := range order {
		if g, ok := open[id]; ok {
			return g, true
		}
	}
	return GateEvent{}, false
}

// last returns the most recent gate decision of the given type.
func (t *Task) last(kind string) (GateEvent, bool) {
	for i := len(t.Gates) - 1; i >= 0; i-- {
		if t.Gates[i].Type == kind {
			return t.Gates[i], true
		}
	}
	return GateEvent{}, false
}

// LastReport is the latest task.reported, if any.
func (t *Task) LastReport() (Report, bool) {
	if len(t.Reports) == 0 {
		return Report{}, false
	}
	return t.Reports[len(t.Reports)-1], true
}

// LastEvidence is the latest evidence recorded under ref.
func (t *Task) LastEvidence(ref string) (Recorded, bool) {
	for i := len(t.Evidence) - 1; i >= 0; i-- {
		if t.Evidence[i].Ref == ref {
			return t.Evidence[i], true
		}
	}
	return Recorded{}, false
}

// Done reports whether the policy's done gate passed on the task.
func (s *State) Done(t *Task) bool {
	if s.Policy == nil {
		return false
	}
	for _, g := range t.Gates {
		if g.Type == GatePassed && g.Gate == s.Policy.DoneGate() {
			return true
		}
	}
	return false
}

func (s *State) terminal(t *Task) bool { return t.Canceled || s.Done(t) }

// agentMay is every event an agent may be the actor of. A gate decision, an
// acceptance, a cancellation, a brief change or evidence never is.
var agentMay = map[string]bool{
	TaskCreated:   true, // an orchestrator, with a grant in force
	TaskAssigned:  true, // an orchestrator starting its child (checkLineage)
	AttemptEnded:  true, // an orchestrator stopping its own child
	TaskReported:  true, // a child reporting its own task
	GrantIssued:   true, // narrowed for its own child
	GrantRevoked:  true, // a grant it narrowed
	MessageSent:   true, // to its parent or its own child
	PlanProposed:  true, // an orchestrator, for the person to approve
	LeaseClaimed:  true, // for its own task, or (an orchestrator) its agents' tasks
	LeaseReleased: true, // the same
}

// reviewVerdict is a reviewer's verdict, recorded by Lazybox from the review
// store and attributed to the reviewer that filed it: the one evidence an
// agent is the actor of.
func reviewVerdict(e Event) bool {
	if e.Type != EvidenceRecorded {
		return false
	}
	p, ok := decode[Evidence](e)
	return ok && p.Kind == EvidenceReview
}

// Fold replays a mission's log strictly: the first event this Runner would
// refuse is an error. Golden logs and appends use it.
func Fold(events []Event) (*State, error) { return fold(events, true) }

// Replay folds a log leniently: an event that does not apply is kept and
// counted in Ignored, so a mission written by a newer Runner stays readable.
func Replay(events []Event) *State { s, _ := fold(events, false); return s }

// Extend replays a log leniently and folds new events onto it strictly: the
// check an append passes before anything is written.
func Extend(log, next []Event) (*State, error) {
	s := Replay(log)
	for _, e := range next {
		if err := s.apply(e); err != nil {
			return nil, err
		}
	}
	return s, nil
}

func fold(events []Event, strict bool) (*State, error) {
	s := &State{tasks: map[string]*Task{}, goals: map[string]*Goal{}, idem: map[string]int{}}
	for _, e := range events {
		if err := s.apply(e); err != nil {
			if strict {
				return s, err
			}
			s.Events = append(s.Events, e)
			s.Ignored++
		}
	}
	return s, nil
}

// apply validates one event against the state and folds it in. It is the
// single definition of what the log accepts: a refused event changes nothing.
func (s *State) apply(e Event) error {
	if e.Seq != s.Seq+1 {
		return refuse("event %d out of sequence after %d", e.Seq, s.Seq)
	}
	if e.Idempotency != "" {
		if _, dup := s.idem[e.Idempotency]; dup {
			return refuse("idempotency key %q already used", e.Idempotency)
		}
	}
	if e.Type != MissionCreated && s.ID == "" {
		return refuse("the log does not start with mission.created")
	}
	if Known(e) {
		if err := s.fold(e); err != nil {
			return err
		}
	} else {
		s.Ignored++
	}
	s.Seq = e.Seq
	if e.At.After(s.Updated) {
		s.Updated = e.At
	}
	if e.Idempotency != "" {
		s.idem[e.Idempotency] = len(s.Events)
	}
	s.Events = append(s.Events, e)
	return nil
}

func (s *State) fold(e Event) error {
	if e.Type == MissionCreated {
		if s.ID != "" || e.Seq != 1 {
			return refuse("mission.created must be the first event")
		}
		p, ok := decode[MissionPayload](e)
		if !ok || p.Title == "" || e.Mission == "" {
			return refuse("a mission needs an id and a title")
		}
		s.ID, s.Title, s.Brief, s.Created = e.Mission, p.Title, p.Brief, e.At
		return nil
	}
	if e.Mission != s.ID {
		return refuse("event of mission %q in the log of %q", e.Mission, s.ID)
	}
	if s.Canceled {
		return refuse("the mission is canceled")
	}
	if (e.Type == GatePassed || e.Type == GateRejected) && e.Actor.Kind != ActorHuman {
		// Human gates are owner-only by construction: no role, rule or agent passes one.
		return refuse("only a person passes or rejects a gate")
	}
	if e.Actor.Kind == ActorAgent && !agentMay[e.Type] && !reviewVerdict(e) {
		// What an agent may write is a short list; every other change is a person's or Lazybox's.
		return refuse("an agent cannot record %s", e.Type)
	}
	switch e.Type {
	case GrantIssued, GrantRevoked, MessageSent:
		return s.foldGrant(e)
	case PlanProposed, PlanApproved, PlanRejected, BudgetRaised, BudgetWarning, BudgetExhausted:
		return s.foldPlan(e)
	case LeaseClaimed, LeaseReleased, OverlapDetected, WIPHeld:
		return s.foldLease(e)
	case MissionProcessPinned:
		p, ok := decode[ProcessPayload](e)
		if !ok {
			return refuse("process payload required")
		}
		policy, err := policyOf(p)
		if err != nil {
			return err
		}
		s.Policy, s.Process = policy, p
	case MissionBriefChanged:
		p, ok := decode[MissionPayload](e)
		if !ok || (p.Title == "" && p.Brief == "") {
			return refuse("a new title or brief required")
		}
		if p.Title != "" {
			s.Title = p.Title
		}
		s.Brief = p.Brief
	case MissionCanceled:
		p, _ := decode[CancelPayload](e)
		s.Canceled, s.Reason = true, p.Reason
	case GoalCreated:
		p, ok := decode[GoalPayload](e)
		if !ok || p.Goal == "" || p.Title == "" {
			return refuse("a goal needs an id and a title")
		}
		if s.goals[p.Goal] != nil {
			return refuse("goal %q exists", p.Goal)
		}
		g := &Goal{ID: p.Goal, Title: p.Title, Created: e.At}
		s.goals[g.ID] = g
		s.Goals = append(s.Goals, g)
	case TaskCreated:
		p, ok := decode[TaskPayload](e)
		if !ok || p.Task == "" || p.Title == "" {
			return refuse("a task needs an id and a title")
		}
		if s.tasks[p.Task] != nil {
			return refuse("task %q exists", p.Task)
		}
		if p.Goal != "" && s.goals[p.Goal] == nil {
			return refuse("goal %q does not exist", p.Goal)
		}
		if e.Actor.Kind == ActorAgent && s.HeldBy(e.Actor.Conversation) == nil {
			return refuse("only an orchestrator with a grant in force adds tasks")
		}
		for _, before := range p.BlockedBy {
			if s.tasks[before] == nil {
				return refuse("task %q does not exist", before)
			}
		}
		t := &Task{ID: p.Task, Goal: p.Goal, Title: p.Title, Brief: p.Brief, BlockedBy: append([]string(nil), p.BlockedBy...), Created: e.At, Updated: e.At}
		s.tasks[t.ID] = t
		s.Tasks = append(s.Tasks, t)
	default:
		return s.foldTask(e)
	}
	return nil
}

// foldTask folds the events whose subject is one task.
func (s *State) foldTask(e Event) error {
	var id string
	switch e.Type {
	case TaskBriefChanged:
		p, _ := decode[TaskPayload](e)
		id = p.Task
	case TaskEdgeAdded:
		p, _ := decode[EdgePayload](e)
		id = p.Task
	case TaskCanceled:
		p, _ := decode[CancelPayload](e)
		id = p.Task
	case TaskAssigned, AttemptStarted, AttemptEnded:
		p, _ := decode[AttemptPayload](e)
		id = p.Task
	case TaskReported:
		p, _ := decode[ReportPayload](e)
		id = p.Task
	case EvidenceRecorded:
		p, _ := decode[Evidence](e)
		id = p.Task
	case GateRequested, GatePassed, GateRejected:
		p, _ := decode[GatePayload](e)
		id = p.Task
	}
	t := s.tasks[id]
	if t == nil {
		return refuse("task %q does not exist", id)
	}
	if e.Subject != TaskSubject(id) {
		return refuse("subject %q does not name task %q", e.Subject, id)
	}
	if s.terminal(t) && e.Type != AttemptEnded {
		// A done or canceled task takes no late report, evidence or assignment
		// (from a dead agent or anyone else). Its attempts may still end.
		return refuse("task %q is %s", t.Title, map[bool]string{true: "canceled", false: "done"}[t.Canceled])
	}
	switch e.Type {
	case TaskBriefChanged:
		p, _ := decode[TaskPayload](e)
		if p.Title == "" && p.Brief == "" {
			return refuse("a new title or brief required")
		}
		if p.Title != "" {
			t.Title = p.Title
		}
		t.Brief = p.Brief
	case TaskEdgeAdded:
		p, _ := decode[EdgePayload](e)
		if s.tasks[p.After] == nil || p.After == t.ID {
			return refuse("an edge needs another existing task")
		}
		switch p.Kind {
		case "blocks":
			if s.reaches(p.After, t.ID) {
				return refuse("that edge would make a cycle")
			}
			t.BlockedBy = appendOnce(t.BlockedBy, p.After)
		case "merge_after":
			t.MergeAfter = appendOnce(t.MergeAfter, p.After)
		default:
			return refuse("edge kind is blocks or merge_after")
		}
	case TaskCanceled:
		p, _ := decode[CancelPayload](e)
		t.Canceled, t.CancelReason = true, p.Reason
	case TaskAssigned:
		p, _ := decode[AttemptPayload](e)
		if p.Attempt == "" || p.Conversation == "" {
			return refuse("an assignment needs an attempt and a conversation")
		}
		if current := t.Current(); current != nil && !current.Ended {
			return refuse("task %q already has a live attempt; end it first", t.Title)
		}
		for _, a := range t.Attempts {
			if a.ID == p.Attempt {
				return refuse("attempt %q exists", p.Attempt)
			}
		}
		if e.Actor.Kind == ActorAgent || p.Grant != "" || p.Parent != "" {
			if err := s.checkLineage(e, p); err != nil {
				return err
			}
		}
		t.Attempts = append(t.Attempts, &Attempt{ID: p.Attempt, Task: t.ID, Conversation: p.Conversation, Workspace: p.Workspace, Execution: p.Execution, New: p.New, Parent: p.Parent, Grant: p.Grant, Role: p.Role, Permission: p.Permission, Profile: p.Profile})
	case AttemptStarted:
		p, _ := decode[AttemptPayload](e)
		a := t.Current()
		if a == nil || a.ID != p.Attempt || a.Ended || !a.Started.IsZero() {
			return refuse("attempt %q was not assigned or already started", p.Attempt)
		}
		a.Started, a.StartSeq = e.At, e.Seq
		if p.Execution != "" {
			a.Execution = p.Execution
		}
	case AttemptEnded:
		p, _ := decode[AttemptPayload](e)
		a := t.attempt(p.Attempt)
		if a == nil || a.Ended {
			return refuse("attempt %q is not live", p.Attempt)
		}
		if e.Actor.Kind == ActorAgent && (a.Parent == "" || a.Parent != e.Actor.Conversation) {
			return refuse("an agent stops only the children it started")
		}
		a.Ended, a.EndedAt, a.Outcome = true, e.At, p.Outcome
	case TaskReported:
		p, _ := decode[ReportPayload](e)
		if _, open := t.OpenGate(); open {
			return refuse("task %q is already waiting for a decision", t.Title)
		}
		if len(p.Summary) > maxSummary {
			return refuse("report summary is longer than %d bytes", maxSummary)
		}
		if e.Actor.Kind == ActorAgent {
			// An agent reports only the task it is the live attempt of.
			if a := t.Current(); a == nil || a.Ended || a.Conversation != e.Actor.Conversation {
				return refuse("an agent reports only its own task")
			}
		}
		t.Reports = append(t.Reports, Report{Seq: e.Seq, At: e.At, Summary: p.Summary, Actor: e.Actor, Evidence: append([]string(nil), e.Evidence...)})
	case EvidenceRecorded:
		p, _ := decode[Evidence](e)
		if p.Ref == "" || (p.Kind != EvidenceRun && p.Kind != EvidenceCommit && p.Kind != EvidencePR && p.Kind != EvidenceReview) {
			return refuse("evidence needs a kind and a reference")
		}
		if p.Corrects != 0 {
			if e.Actor.Kind != ActorLazybox || p.Kind != EvidenceReview || p.Outcome != OutcomeUnknown {
				return refuse("only Lazybox retracts invalid review evidence")
			}
			found := false
			for i := range t.Evidence {
				previous := &t.Evidence[i]
				if previous.Seq == p.Corrects && previous.Kind == EvidenceReview && previous.Ref == p.Ref && previous.Attempt == p.Attempt {
					previous.Outcome, previous.Detail, previous.State = p.Outcome, p.Detail, p.State
					found = true
					break
				}
			}
			if !found {
				return refuse("review correction does not identify existing evidence")
			}
		} else {
			t.Evidence = append(t.Evidence, Recorded{Seq: e.Seq, At: e.At, Actor: e.Actor, Evidence: p})
		}
	case GateRequested:
		p, _ := decode[GatePayload](e)
		if p.Gate == "" {
			return refuse("a gate needs an id")
		}
		if g, open := t.OpenGate(); open && g.Gate == p.Gate {
			return refuse("gate %q is already open", p.Gate)
		}
		t.Gates = append(t.Gates, GateEvent{Seq: e.Seq, At: e.At, Type: e.Type, Gate: p.Gate, Note: p.Note, Actor: e.Actor})
	case GatePassed, GateRejected:
		p, _ := decode[GatePayload](e)
		if g, open := t.OpenGate(); !open || g.Gate != p.Gate {
			return refuse("gate %q is not open on task %q", p.Gate, t.Title)
		}
		if len(p.Note) > maxNote {
			return refuse("note is longer than %d bytes", maxNote)
		}
		t.Gates = append(t.Gates, GateEvent{Seq: e.Seq, At: e.At, Type: e.Type, Gate: p.Gate, Note: p.Note, Actor: e.Actor})
	}
	t.Updated = e.At
	return nil
}

func (t *Task) attempt(id string) *Attempt {
	for _, a := range t.Attempts {
		if a.ID == id {
			return a
		}
	}
	return nil
}

// reaches reports whether from is blocked, directly or not, by to.
func (s *State) reaches(from, to string) bool {
	seen := map[string]bool{}
	var walk func(string) bool
	walk = func(id string) bool {
		if id == to {
			return true
		}
		if seen[id] {
			return false
		}
		seen[id] = true
		if t := s.tasks[id]; t != nil {
			for _, next := range t.BlockedBy {
				if walk(next) {
					return true
				}
			}
		}
		return false
	}
	return walk(from)
}

func appendOnce(list []string, value string) []string {
	for _, v := range list {
		if v == value {
			return list
		}
	}
	return append(list, value)
}
