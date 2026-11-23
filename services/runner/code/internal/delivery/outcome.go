package delivery

import (
	"crypto/sha256"
	"encoding/hex"
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
	Unbound    = "unbound"
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

// CandidateID names the revisions under test: each workspace repository and its
// commit. The same revisions always yield the same ID; any change yields another.
func CandidateID(revisions map[string]string) string {
	members := make([]string, 0, len(revisions))
	for member := range revisions {
		members = append(members, member)
	}
	slices.Sort(members)
	h := sha256.New()
	for _, member := range members {
		fmt.Fprintf(h, "%s=%s\n", member, revisions[member])
	}
	return "candidate:" + hex.EncodeToString(h.Sum(nil))[:16]
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
	// Prerequisite is failed when something the check needs kept it from running.
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
	case !slices.Contains([]string{"", Failed}, o.Prerequisite):
		return fmt.Errorf("unknown prerequisite %q", o.Prerequisite)
	}
	switch o.Source {
	case SourceHuman:
		if !slices.Contains([]string{Accepted, Waived, Rejected}, o.Decision) || o.Assertion != "" || o.Prerequisite != "" || o.ExitCode != nil {
			return errors.New("a human observation records only a decision")
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
	case o.Prerequisite == Failed && o.Assertion != "":
		return errors.New("a check that could not run asserts nothing")
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
// else, a bare exit 0 or a pipe included, leaves the criterion unverified.
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
// criterion takes its latest observation, so only a later observation of the
// same criterion resolves an earlier failure. The verdict is the worst required
// state: failed, then blocked, then incomplete; passed when every required
// criterion passed; unverified with no required criterion or an unbound contract.
func Assess(scope Scope, contract Contract, history []Observation) Assessment {
	a := Assessment{Verdict: Unverified, Criteria: map[string]string{}}
	for _, c := range contract.Criteria {
		a.Criteria[c.ID] = Unrun
	}
	if !contract.Bound() {
		for id := range a.Criteria {
			a.Criteria[id] = Unbound
		}
		return a
	}
	if scope.ContractDigest != contract.Digest || scope.Candidate == "" || scope.Attempt == "" {
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
		if _, ok := a.Criteria[o.CriterionID]; ok {
			a.Criteria[o.CriterionID] = o.Classify()
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
