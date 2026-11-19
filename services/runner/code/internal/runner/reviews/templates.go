package reviews

import "fmt"

const (
	CodeReviewTemplate     = "code-review-v1"
	SecurityReviewTemplate = "security-review-v1"
	VerificationTemplate   = "behavior-verification-v1"
)

// Template describes a read-only sibling review.
type Template struct {
	Title    string
	Guidance string
}

// TemplateFor resolves a persisted template revision.
func TemplateFor(id string) (Template, error) {
	switch id {
	case "", CodeReviewTemplate:
		return Template{Title: "Review"}, nil
	case VerificationTemplate:
		return Template{Title: "Behavior verification", Guidance: verificationGuidance}, nil
	case SecurityReviewTemplate:
		return Template{Title: "Security review", Guidance: "Perform a defensive security review. Focus on changed trust boundaries, authorization, data exposure, input handling and secrets. Report actionable findings with evidence and remediation. State which parts of the scope you inspected and any coverage gaps. Use static inspection and safe defensive checks."}, nil
	default:
		return Template{}, fmt.Errorf("unknown conversation template %q", id)
	}
}

// verificationGuidance also applies to customized review briefs.
const verificationGuidance = `Independently verify the behavior promised by the goal against the captured changes.

Before reading the author's explanation, derive acceptance criteria from the goal and identify ambiguities. Choose at least one behavioral check independently of the author's tests. Read the code to guide investigation and check whether changed tests weaken existing assertions.

For each criterion, report passed, failed, or unverified. Include expected and observed behavior, the captured revision, the check or command, and a reference to its output. Distinguish observed execution from static inspection. Author-provided command results are background evidence.

For a bug fix, use the same scenario and assertions on base and candidate when supported. Record both results. For a refactor, compare representative inputs and identify intended differences. Explore a neighboring failure case such as retries, empty state, restart, or concurrent activity. Identify useful scenarios to retain as regression tests.

Use existing managed verification commands within your read-only permissions. The live checkout may differ from the captured commit. Only attribute a runtime result to the candidate when the execution environment identifies that captured revision and preserves it during the check. If an isolated execution path is unavailable, record runtime verification as unverified and explain the missing capability. Keep repository contents and shared services intact.

File actionable defects through the Review tray with reproduction evidence. In your final reply, provide the criterion-by-criterion evidence report and coverage gaps, including snapshot omissions. Treat unresolved intent as a question for the person. A lack of findings alone does not establish that acceptance criteria passed.`
