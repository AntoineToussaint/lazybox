package delivery

import "fmt"

// Check is what `lazybox run --check C` or `--needs C` declared, resolved when
// the run registered: the criterion, and the contract and attempt it counts
// for. It is persisted with the command, so a run is judged against the
// binding in force when it was declared, even after a Runner restart.
type Check struct {
	CriterionID string `json:"criterionId"`
	// Prerequisite: the run is something the criterion's check needs (--needs),
	// not the check itself (--check).
	Prerequisite   bool    `json:"prerequisite,omitempty"`
	ContractDigest string  `json:"contractDigest"`
	Attempt        string  `json:"attempt"`
	Subject        Subject `json:"subject"`
	// Contract names the spec revision, for what a person and an agent read.
	Artifact string `json:"artifact"`
	Title    string `json:"title,omitempty"`
	Version  uint32 `json:"version"`
	// Candidate is the workspace's files when the run registered
	// (CandidateID); a run that finishes on other files records nothing.
	Candidate string `json:"candidate"`
}

// A shell's status for a command it could not start: not executable (126) or not found (127).
const (
	exitNotExecutable = 126
	exitNotFound      = 127
)

// Observe reads a finished run as an observation of its criterion.
//
//   - A --check run that exits 0 asserts a pass, and any other exit a failure.
//   - A --needs run that exits non-zero is a failed prerequisite; exit 0 is a
//     met one, which clears a blocked criterion and proves nothing else.
//   - A command that could not start (126, 127) is a failed prerequisite either way.
//   - A run ended by a signal (a stop) or with no observed exit (a pipe) asserts nothing.
func (c Check) Observe(id, reference, candidate string, exit *int, signalled bool) Observation {
	o := Observation{Schema: ObservationSchema, ID: id, Scope: Scope{ContractDigest: c.ContractDigest, Candidate: candidate, Attempt: c.Attempt},
		Subject: c.Subject, CriterionID: c.CriterionID, Source: SourceCommand, Reference: reference, ExitCode: exit}
	switch {
	case exit == nil || signalled:
	case *exit == exitNotExecutable || *exit == exitNotFound:
		o.Prerequisite = Failed
	case c.Prerequisite && *exit != 0:
		o.Prerequisite = Failed
	case c.Prerequisite:
		o.Prerequisite = Passed
	case *exit == 0:
		o.Assertion = Passed
	default:
		o.Assertion = Failed
	}
	return o
}

// Summary is the one line a run's output ends with: what it recorded.
func (o Observation) Summary(c Check) string {
	state, role := o.Classify(), "check"
	if o.Source == SourceReview {
		role = "review"
	}
	if c.Prerequisite {
		role = "prerequisite"
		if o.Prerequisite == Passed {
			state = "met; run its --check next"
		}
	}
	return fmt.Sprintf("%s %s: %s on %s (%s)", role, o.CriterionID, state, o.Candidate, c.SpecName())
}

// SpecName names the spec revision a check counts for: its title when known.
func (c Check) SpecName() string { return specName(c.Title, c.Artifact, c.Version) }

func specName(title, artifact string, version uint32) string {
	if title != "" {
		return fmt.Sprintf("“%s” v%d", title, version)
	}
	return fmt.Sprintf("spec %s v%d", artifact, version)
}
