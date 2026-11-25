package reviews

import (
	"errors"
	"fmt"
	"slices"
	"strings"
)

// VerificationCheck is an independently chosen acceptance criterion and its
// immutable command recipe. Each round retains both execution receipts.
//
// A check of Mode ModeTest is a reviewer's test run instead: any review's
// reviewer runs a command against the captured candidate only, in the same
// disposable, writable checkout, and its receipt is recorded in the review.
// It is evidence, not an acceptance criterion (VerificationComplete).
type VerificationCheck struct {
	Name       string                  `json:"name"`
	Mode       string                  `json:"mode,omitempty"`
	Member     string                  `json:"member"`
	Expected   string                  `json:"expected"`
	Argv       []string                `json:"argv"`
	Regression string                  `json:"regression,omitempty"`
	Runs       []VerificationExecution `json:"runs,omitempty"`
}

// VerificationExecution binds managed command output to captured revisions.
type VerificationExecution struct {
	IsolationVersion int    `json:"isolationVersion,omitempty"`
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

// ModeTest marks a reviewer's test run: candidate only, any review template.
const ModeTest = "test"

// TestExpected is the expected behaviour a test run states when its reviewer
// names none.
const TestExpected = "The command exits 0"

// Test reports whether the check is a reviewer's test run.
func (c VerificationCheck) Test() bool { return c.Mode == ModeTest }

// Validate checks the criterion before the Runner starts any process.
func (c VerificationCheck) Validate() error {
	if c.Mode != "" && c.Mode != ModeTest {
		return errors.New("verification mode is acceptance (empty) or test")
	}
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

// TestSummary says how the reviewer's test runs ended, by their latest
// receipt ("2 tests passed, 1 failed"); "" when it ran none.
func TestSummary(run Run) string {
	counts := map[string]int{}
	total := 0
	for _, check := range run.Verification {
		if check.Test() && len(check.Runs) > 0 {
			counts[check.Runs[len(check.Runs)-1].State]++
			total++
		}
	}
	if total == 0 {
		return ""
	}
	var parts []string
	for _, state := range []string{"passed", "failed", "running", "preparing", "unverified"} {
		if n := counts[state]; n > 0 {
			parts = append(parts, fmt.Sprintf("%d %s", n, state))
		}
	}
	noun := "tests"
	if total == 1 {
		noun = "test"
	}
	return fmt.Sprintf("%d %s: %s", total, noun, strings.Join(parts, ", "))
}

// VerificationComplete is true only with executed acceptance checks for this
// round. Missing, interrupted and older receipts cannot establish acceptance.
func VerificationComplete(run Run) bool {
	if len(run.Input) == 0 {
		return false
	}
	for _, input := range run.Input {
		if input.Skipped > 0 {
			return false
		}
	}
	acceptance := 0
	for _, check := range run.Verification {
		if check.Test() {
			continue
		}
		acceptance++
		if len(check.Runs) == 0 {
			return false
		}
		result := check.Runs[len(check.Runs)-1]
		input := slices.IndexFunc(run.Input, func(i ReviewInput) bool { return i.Member == check.Member })
		if input < 0 || result.IsolationVersion != 1 || result.Base == "" || result.Candidate == "" || result.Base != run.Input[input].Base || result.Candidate != run.Input[input].Commit || result.Round != run.Round || result.State != "passed" {
			return false
		}
	}
	return acceptance > 0
}
