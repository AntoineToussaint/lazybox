package reviews

import "fmt"

const (
	ReviewPRCITemplate     = "review-pr-ci-v1"
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
	case ReviewPRCITemplate:
		return Template{Title: "Review for pull request", Guidance: "Review the captured changes and file actionable findings with their evidence. The author will apply fixes and handle the pull request and CI after your report."}, nil
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

Use the CLI lazybox review verify --name NAME --member REPOSITORY --expected TEXT --regression TEXT -- COMMAND ARGS... or lazybox_command with action verify, name (a stable criterion name), member (workspace repository name), expected (the observable acceptance condition), and argv (the executable and arguments). The Runner executes that exact command in independent base and candidate checkouts and returns their managed command IDs. Use read and wait to inspect both outputs. Commands must assert the expected behavior and exit nonzero when it fails. Setup and dependencies must be included in the recipe when needed. Keep commands scoped to their checkout and avoid external side effects. The recipe is retained and rerun after author repairs. Reusing the name returns the existing execution, so do not start duplicates. Explain infrastructure failures explicitly and retain them as unverified.

Every failed candidate check needs an actionable blocker finding with reproduction evidence so the author can repair it. For a confirmed bug, ask the author to retain the reproducer as a regression test and verify that test on the next round. The Runner ends a review successfully only when all recorded checks in the current round pass and blockers are resolved. If the criteria are ambiguous, ask the person to decide and state the question in the report.

File actionable defects through the Review tray with reproduction evidence. In your final reply, provide the criterion-by-criterion evidence report and coverage gaps, including snapshot omissions. Treat unresolved intent as a question for the person. A lack of findings alone does not establish that acceptance criteria passed.`
