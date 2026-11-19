package reviews

import (
	"errors"
	"strings"
)

// VerificationCheck is an independently chosen acceptance criterion and its
// immutable command recipe. Each round retains both execution receipts.
type VerificationCheck struct {
	Name       string                  `json:"name"`
	Member     string                  `json:"member"`
	Expected   string                  `json:"expected"`
	Argv       []string                `json:"argv"`
	Regression string                  `json:"regression,omitempty"`
	Runs       []VerificationExecution `json:"runs,omitempty"`
}

// VerificationExecution binds managed command output to captured revisions.
type VerificationExecution struct {
	Round            int    `json:"round"`
	Base             string `json:"base"`
	Candidate        string `json:"candidate"`
	BaseCommand      string `json:"baseCommand,omitempty"`
	CandidateCommand string `json:"candidateCommand,omitempty"`
	BaseExit         *int   `json:"baseExit,omitempty"`
	CandidateExit    *int   `json:"candidateExit,omitempty"`
	State            string `json:"state"`
	Detail           string `json:"detail,omitempty"`
}

// Validate checks the criterion before the Runner starts any process.
func (c VerificationCheck) Validate() error {
	if strings.TrimSpace(c.Name) == "" || len(c.Name) > 160 || strings.TrimSpace(c.Expected) == "" || len(c.Expected) > 4096 {
		return errors.New("verification needs a name (up to 160 bytes) and expected behavior (up to 4096 bytes)")
	}
	if c.Member == "" || len(c.Argv) == 0 || len(c.Argv) > 128 || c.Argv[0] == "" {
		return errors.New("verification needs a workspace repository and a command argv")
	}
	if len(c.Regression) > 8192 {
		return errors.New("regression proposal exceeds 8 KiB")
	}
	size := 0
	for _, arg := range c.Argv {
		size += len(arg)
		if strings.ContainsRune(arg, 0) {
			return errors.New("verification arguments contain a NUL")
		}
	}
	if size > 64<<10 {
		return errors.New("verification command exceeds 64 KiB")
	}
	return nil
}

// VerificationComplete is true only with executed acceptance checks for this
// round. Missing, interrupted and older receipts cannot establish acceptance.
func VerificationComplete(run Run) bool {
	for _, input := range run.Input {
		if input.Skipped > 0 {
			return false
		}
	}
	if len(run.Verification) == 0 {
		return false
	}
	for _, check := range run.Verification {
		if len(check.Runs) == 0 {
			return false
		}
		result := check.Runs[len(check.Runs)-1]
		if result.Round != run.Round || result.State != "passed" {
			return false
		}
	}
	return true
}
