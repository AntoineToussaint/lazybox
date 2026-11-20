package work

import (
	"fmt"
	"slices"
)

const (
	CandidateReserved = "candidate.reserved"
	CandidateReady    = "candidate.ready"
	CandidateChecked  = "candidate.checked"
	CandidateAccepted = "candidate.accepted"
)

// Candidate records immutable inputs to a combined check. Its ID also names
// the dedicated workspace. Generation is the manifest it was built against.
type Candidate struct {
	ID         string
	Plan       string
	Generation uint64
	State      string
	RepairTask string
	Command    string
	Detail     string
	Check      []string
	Members    []BaselineMember
	Reviews    map[string]string // task to reviewed evidence event ID
}

// IntegrationReview selects the current worker’s latest contract-bound verdict.
func (s *State) IntegrationReview(t *Task) *Recorded {
	if t == nil || t.Canceled || t.Current() == nil {
		return nil
	}
	if _, open := t.OpenGate(); !open && !s.Done(t) {
		return nil
	}
	for i := len(t.Evidence) - 1; i >= 0; i-- {
		r := &t.Evidence[i]
		if r.Attempt == t.Current().ID && r.Kind == EvidenceReview && r.ContractDigest != "" {
			if r.Outcome == OutcomePass && !reviewStale(t, r) && r.Blockers == 0 {
				return r
			}
			return nil
		}
	}
	return nil
}

func (r *Recorded) ReviewRevision() string { return fmt.Sprintf("%s/%d", r.Ref, r.Seq) }

func (s *State) Candidate(id string) *Candidate {
	for i := range s.Candidates {
		if s.Candidates[i].ID == id {
			return &s.Candidates[i]
		}
	}
	return nil
}
func (s *State) foldCandidate(e Event) error {
	p, ok := decode[Candidate](e)
	if !ok || p.ID == "" {
		return refuse("candidate ID required")
	}
	c := s.Candidate(p.ID)
	switch e.Type {
	case CandidateReserved:
		if c != nil || p.Generation != s.Generation || p.State != "preparing" || len(p.Check) == 0 || len(p.Reviews) == 0 {
			return refuse("invalid candidate reservation")
		}
		s.Candidates = append(s.Candidates, p)
	case CandidateReady:
		if c == nil || c.State != "preparing" || len(p.Members) == 0 {
			return refuse("invalid candidate manifest")
		}
		c.State, c.Members = "checking", p.Members
	case CandidateChecked:
		if c == nil || (c.State != "preparing" && c.State != "checking") || (p.State != "passed" && p.State != "failed") {
			return refuse("invalid candidate result")
		}
		if p.State == "passed" && (c.State != "checking" || p.Command == "" || !slices.Equal(c.Members, p.Members)) {
			return refuse("missing combined check evidence")
		}
		c.RepairTask = p.RepairTask
		c.State, c.Detail, c.Command, c.Members = p.State, p.Detail, p.Command, p.Members
	case CandidateAccepted:
		if e.Actor.Kind != ActorHuman || c == nil || c.State != "passed" || c.Generation != s.Generation {
			return refuse("candidate must pass on the current generation")
		}
		for task, ref := range c.Reviews {
			r := s.IntegrationReview(s.Task(task))
			if r == nil || r.ReviewRevision() != ref {
				return refuse("review for task %s has changed", task)
			}
		}
		c.State = "accepted"
		s.Generation++
		s.AcceptedCandidate = c.ID
	}
	return nil
}
