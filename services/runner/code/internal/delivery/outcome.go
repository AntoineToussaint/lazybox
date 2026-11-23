package delivery

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"
)

// ObservationSchema versions the recorded observation. A change to its fields
// or to how Classify reads them is a new version, with its own fixtures.
const ObservationSchema = "delivery-observation.v1"

// Evidence sources. A command is a managed `lazybox run`; a review is an agent
// review execution; a human records a decision and never a technical result.
const (
	SourceCommand = "command"
	SourceReview  = "review"
	SourceHuman   = "human"
)

// Criterion states and verdicts.
const (
	Passed     = "passed"
	Failed     = "failed"
	Blocked    = "blocked"
	Incomplete = "incomplete"
	Unverified = "unverified"
	Unrun      = "unrun"
)

// Human decisions. They are recorded beside the technical verdict and never replace it.
const (
	Accepted = "accepted"
	Waived   = "waived"
	Rejected = "rejected"
)

// Contract pins the criteria an outcome is judged against. A spec.v1 contract
// has text criteria and no stable IDs: its criteria stay unbound, and the
// verdict unverified, until a spec.v2 version binds them.
type Contract struct {
	Artifact string      `json:"artifact"`
	Version  uint32      `json:"version"`
	Schema   string      `json:"schema"`
	Digest   string      `json:"digest"`
	Criteria []Criterion `json:"criteria"`
	// Legacy holds a spec.v1 contract's text criteria.
	Legacy []string `json:"legacy,omitempty"`
}

// Bound reports whether observations can be attributed to the contract's criteria.
func (c Contract) Bound() bool { return c.Schema == "spec.v2" && len(c.Criteria) > 0 }

// CandidateID names the files under test: each workspace repository and its
// revision (its commit, and its uncommitted tree when it has one). The same
// revisions always yield the same ID; any change yields another.
func CandidateID(revisions map[string]string) string {
	members := make([]string, 0, len(revisions))
	for member := range revisions {
		members = append(members, member)
	}
	slices.Sort(members)
	pairs := make([][2]string, 0, len(members))
	for _, member := range members {
		pairs = append(pairs, [2]string{member, revisions[member]})
	}
	// JSON keeps member and revision apart whatever characters they hold.
	encoded, _ := json.Marshal(pairs)
	sum := sha256.Sum256(encoded)
	return "candidate:" + hex.EncodeToString(sum[:])[:16]
}

// Scope identifies an immutable contract and candidate within one execution attempt.
type Scope struct {
	ContractDigest string `json:"contractDigest"`
	Candidate      string `json:"candidate"`
	Attempt        string `json:"attempt"`
}

// Subject places the work: always a conversation, and a Mission task when the
// conversation works one. Lookups by conversation and by task find the same observations.
type Subject struct {
	Conversation string `json:"conversation"`
	Mission      string `json:"mission,omitempty"`
	Task         string `json:"task,omitempty"`
}

// Observation is one piece of evidence about one criterion, as an evidence
// adapter records it. Process exit, assertion, prerequisite and human decision
// are separate facts: a command that exits 0 asserts nothing by itself.
type Observation struct {
	Schema string `json:"schema"`
	// ID is the adapter's idempotency key: a replay of the same evidence carries the same ID.
	ID string `json:"id"`
	Scope
	Subject     Subject `json:"subject"`
	CriterionID string  `json:"criterionId,omitempty"`
	Source      string  `json:"source"`
	// Reference names the evidence: run:<execution>/<command>, review:<id>@<round>, or the person.
	Reference string `json:"reference"`
	// ExitCode is the process's own status, absent when it was not observed (a pipe).
	ExitCode *int `json:"exitCode,omitempty"`
	// Assertion is the declared check's result: passed, failed, or empty when nothing was asserted.
	Assertion string `json:"assertion,omitempty"`
	// Prerequisite is failed when something the check needs kept it from
	// running, and passed when that step succeeded (`--needs` exiting 0).
	Prerequisite string `json:"prerequisite,omitempty"`
	// Decision is a person's accepted, waived or rejected; only a human source carries one.
	Decision string `json:"decision,omitempty"`
}

// Validate refuses an observation whose facts cannot be read unambiguously.
func (o Observation) Validate() error {
	switch {
	case o.Schema != ObservationSchema:
		return fmt.Errorf("observation schema %q is not %s", o.Schema, ObservationSchema)
	case strings.TrimSpace(o.ID) == "" || strings.TrimSpace(o.Reference) == "":
		return errors.New("observation needs an ID and a reference")
	case o.ContractDigest == "" || o.Candidate == "" || o.Attempt == "":
		return errors.New("observation needs a contract digest, candidate and attempt")
	case o.Subject.Conversation == "" || (o.Subject.Task != "") != (o.Subject.Mission != ""):
		return errors.New("observation needs a conversation, and a Mission and task together")
	case !slices.Contains([]string{"", Passed, Failed}, o.Assertion):
		return fmt.Errorf("unknown assertion %q", o.Assertion)
	case !slices.Contains([]string{"", Passed, Failed}, o.Prerequisite):
		return fmt.Errorf("unknown prerequisite %q", o.Prerequisite)
	}
	switch o.Source {
	case SourceHuman:
		// A decision is on the whole outcome, never on one criterion.
		if !slices.Contains([]string{Accepted, Waived, Rejected}, o.Decision) || o.Assertion != "" || o.Prerequisite != "" || o.ExitCode != nil || o.CriterionID != "" {
			return errors.New("a human observation records only a decision on the outcome")
		}
		return nil
	case SourceCommand, SourceReview:
	default:
		return fmt.Errorf("unknown source %q", o.Source)
	}
	switch {
	case o.Decision != "":
		return errors.New("only a person records a decision")
	case o.CriterionID == "":
		return errors.New("a technical observation names its criterion")
	case o.Prerequisite != "" && o.Assertion != "":
		return errors.New("a prerequisite asserts nothing about its criterion")
	case o.Source == SourceReview && o.Prerequisite != "":
		return errors.New("a review is no prerequisite")
	case o.Prerequisite == Failed && o.ExitCode != nil && *o.ExitCode == 0:
		return errors.New("a failed prerequisite contradicts a successful exit")
	case o.Prerequisite == Passed && (o.ExitCode == nil || *o.ExitCode != 0):
		return errors.New("a prerequisite passes only with exit 0 observed")
	case o.Source == SourceReview && o.ExitCode != nil:
		return errors.New("a review has no process exit")
	case o.Source == SourceCommand && o.Assertion == Passed && o.ExitCode != nil && *o.ExitCode != 0:
		return errors.New("a passing assertion contradicts a failing exit")
	}
	return nil
}

// Classify reads one valid technical observation as a criterion state.
// A failed assertion fails; a failed prerequisite blocks; a passing assertion
// passes only with its exit observed as 0 (commands) or from a review. Anything
// else, a bare exit 0, a met prerequisite or a stopped run, is unverified.
func (o Observation) Classify() string {
	switch {
	case o.Assertion == Failed:
		return Failed
	case o.Prerequisite == Failed:
		return Blocked
	case o.Assertion == Passed && (o.Source == SourceReview || o.ExitCode != nil && *o.ExitCode == 0):
		return Passed
	}
	return Unverified
}

type Assessment struct {
	Verdict  string            `json:"verdict"`
	Criteria map[string]string `json:"criteria"`
	// Decision is the latest human decision in scope; the verdict keeps the technical result.
	Decision string `json:"decision,omitempty"`
}

// Assess folds observations in journal order for the requested scope. Each
// criterion takes its latest decisive observation: an assertion or a failed
// prerequisite. An observation that proves nothing (a bare exit 0, a stopped
// run) only marks an unrun criterion unverified, and a met prerequisite only
// clears a blocked one; neither erases a pass or a failure. So only a later
// check of the same criterion resolves an earlier failure. The verdict is the
// worst required state: failed, then blocked, then incomplete; passed when
// every required criterion passed; unverified with no required criterion or
// an unbound (spec.v1) contract.
func Assess(scope Scope, contract Contract, history []Observation) Assessment {
	a := Assessment{Verdict: Unverified, Criteria: map[string]string{}}
	for _, c := range contract.Criteria {
		a.Criteria[c.ID] = Unrun
	}
	if !contract.Bound() || scope.ContractDigest != contract.Digest || scope.Candidate == "" || scope.Attempt == "" {
		return a
	}
	for _, o := range history {
		if o.Scope != scope || o.Validate() != nil {
			continue
		}
		if o.Source == SourceHuman {
			a.Decision = o.Decision
			continue
		}
		current, ok := a.Criteria[o.CriterionID]
		if !ok {
			continue
		}
		switch state := o.Classify(); {
		case o.Assertion != "" || o.Prerequisite == Failed:
			a.Criteria[o.CriterionID] = state
		case o.Prerequisite == Passed && (current == Blocked || current == Unrun):
			a.Criteria[o.CriterionID] = Unverified
		case current == Unrun:
			a.Criteria[o.CriterionID] = Unverified
		}
	}
	rank := 0
	for _, c := range contract.Criteria {
		if !c.Required {
			continue
		}
		verdict, priority := Passed, 1
		switch a.Criteria[c.ID] {
		case Failed:
			verdict, priority = Failed, 4
		case Blocked:
			verdict, priority = Blocked, 3
		case Unrun, Unverified:
			verdict, priority = Incomplete, 2
		}
		if priority > rank {
			a.Verdict, rank = verdict, priority
		}
	}
	return a
}
