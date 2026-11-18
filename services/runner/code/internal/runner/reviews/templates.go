package reviews

import "fmt"

const (
	CodeReviewTemplate     = "code-review-v1"
	SecurityReviewTemplate = "security-review-v1"
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
	case SecurityReviewTemplate:
		return Template{Title: "Security review", Guidance: "Perform a defensive security review. Focus on changed trust boundaries, authorization, data exposure, input handling and secrets. Report actionable findings with evidence and remediation. State which parts of the scope you inspected and any coverage gaps. Use static inspection and safe defensive checks."}, nil
	default:
		return Template{}, fmt.Errorf("unknown conversation template %q", id)
	}
}
