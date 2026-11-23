package delivery

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
)

// OutcomeSchema versions the projected outcome, which is also the body of a
// published verdict snapshot (an artifact of kind report).
const OutcomeSchema = "delivery-outcome.v1"

// Outcome is the delivery outcome of one attempt: the same value whichever
// lookup (its conversation, its Mission task) and whichever surface asks.
type Outcome struct {
	Schema   string   `json:"schema"`
	Contract Contract `json:"contract"`
	Attempt  string   `json:"attempt"`
	Subject  Subject  `json:"subject"`
	// Current is the candidate the workspace holds now; empty when it is
	// gone or cannot be read.
	Current string `json:"current,omitempty"`
	// Decision is the person's decision on the outcome (a Mission task
	// accepted or rejected). It never changes a technical verdict.
	Decision string `json:"decision,omitempty"`
	// Candidates are newest first: the current candidate, then each one the
	// attempt checked, by its latest observation.
	Candidates []CandidateOutcome `json:"candidates"`
}

type CandidateOutcome struct {
	Candidate string             `json:"candidate"`
	Current   bool               `json:"current,omitempty"`
	Verdict   string             `json:"verdict"`
	Criteria  []CriterionOutcome `json:"criteria"`
}

type CriterionOutcome struct {
	ID       string `json:"id"`
	Expected string `json:"expected"`
	Required bool   `json:"required"`
	State    string `json:"state"`
	// Evidence is every observation of the criterion on this candidate, in
	// the order recorded: a failure a later check resolved stays visible.
	Evidence []Evidence `json:"evidence,omitempty"`
}

// Evidence is one observation as a person reads it. Command names the
// managed command a run reference points to; its output is read through
// that command, never from here.
type Evidence struct {
	ID           string `json:"id"`
	Source       string `json:"source"`
	Reference    string `json:"reference"`
	Command      string `json:"command,omitempty"`
	ExitCode     *int   `json:"exitCode,omitempty"`
	Assertion    string `json:"assertion,omitempty"`
	Prerequisite string `json:"prerequisite,omitempty"`
	// State is what this observation alone says (Classify).
	State string `json:"state"`
	// Output is whether the command's full output is still kept, when the
	// projection could tell: "available" or "unavailable".
	Output string `json:"output,omitempty"`
}

// CommandOf is the managed command a run reference (run:<execution>/<command>) names.
func CommandOf(reference string) string {
	rest, ok := strings.CutPrefix(reference, "run:")
	if !ok {
		return ""
	}
	_, command, ok := strings.Cut(rest, "/")
	if !ok {
		return ""
	}
	return command
}

// Project builds the outcome of attempt under contract from the journal.
// current, when known, is the workspace's candidate now; an unchecked
// current candidate leads the list with nothing proven.
func Project(contract Contract, attempt string, subject Subject, current, decision string, history []Observation) Outcome {
	o := Outcome{Schema: OutcomeSchema, Contract: contract, Attempt: attempt, Subject: subject, Current: current, Decision: decision, Candidates: []CandidateOutcome{}}
	last := map[string]int{}
	for i, obs := range history {
		if obs.Attempt == attempt && obs.ContractDigest == contract.Digest && obs.Source != SourceHuman && obs.Validate() == nil {
			last[obs.Candidate] = i
		}
	}
	order := make([]string, 0, len(last)+1)
	for candidate := range last {
		order = append(order, candidate)
	}
	// Newest first, by each candidate's latest observation.
	for i := 1; i < len(order); i++ {
		for j := i; j > 0 && last[order[j]] > last[order[j-1]]; j-- {
			order[j], order[j-1] = order[j-1], order[j]
		}
	}
	if _, seen := last[current]; current != "" && !seen {
		order = append([]string{current}, order...)
	}
	for _, candidate := range order {
		scope := Scope{ContractDigest: contract.Digest, Candidate: candidate, Attempt: attempt}
		a := Assess(scope, contract, history)
		c := CandidateOutcome{Candidate: candidate, Current: candidate == current, Verdict: a.Verdict, Criteria: []CriterionOutcome{}}
		for _, criterion := range contract.Criteria {
			co := CriterionOutcome{ID: criterion.ID, Expected: criterion.Expected, Required: criterion.Required, State: a.Criteria[criterion.ID]}
			for _, obs := range history {
				if obs.Scope == scope && obs.CriterionID == criterion.ID && obs.Validate() == nil {
					co.Evidence = append(co.Evidence, Evidence{ID: obs.ID, Source: obs.Source, Reference: obs.Reference, Command: CommandOf(obs.Reference),
						ExitCode: obs.ExitCode, Assertion: obs.Assertion, Prerequisite: obs.Prerequisite, State: obs.Classify()})
				}
			}
			c.Criteria = append(c.Criteria, co)
		}
		o.Candidates = append(o.Candidates, c)
	}
	return o
}

// Headline is the candidate a surface leads with: the current one when the
// workspace can be read, else the newest checked.
func (o Outcome) Headline() (CandidateOutcome, bool) {
	if len(o.Candidates) == 0 {
		return CandidateOutcome{}, false
	}
	return o.Candidates[0], true
}

// Blocking is the required criteria that keep the headline candidate from
// passing, worst first.
func (c CandidateOutcome) Blocking() []CriterionOutcome {
	var out []CriterionOutcome
	for _, rank := range []string{Failed, Blocked, Unverified, Unrun} {
		for _, criterion := range c.Criteria {
			if criterion.Required && criterion.State == rank {
				out = append(out, criterion)
			}
		}
	}
	return out
}

// ParseOutcome validates a published verdict snapshot: the complete
// document, with no field it does not know.
func ParseOutcome(content string) (Outcome, error) {
	var o Outcome
	dec := json.NewDecoder(bytes.NewReader([]byte(content)))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&o); err != nil {
		return Outcome{}, err
	}
	if dec.More() {
		return Outcome{}, errors.New("a delivery outcome is one JSON object")
	}
	if o.Schema != OutcomeSchema || o.Attempt == "" || o.Contract.Digest == "" || o.Subject.Conversation == "" || o.Candidates == nil {
		return Outcome{}, fmt.Errorf("%s needs a contract, an attempt, a conversation and its candidates", OutcomeSchema)
	}
	for _, c := range o.Candidates {
		if c.Candidate == "" || c.Verdict == "" || len(c.Criteria) != len(o.Contract.Criteria) {
			return Outcome{}, errors.New("each candidate names itself, its verdict and every criterion")
		}
	}
	return o, nil
}
