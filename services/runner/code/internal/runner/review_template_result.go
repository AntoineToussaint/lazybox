package runner

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
)

type reviewResult struct {
	Schema       string                `json:"schema"`
	Run          string                `json:"run"`
	Template     string                `json:"template"`
	Input        []reviews.ReviewInput `json:"input"`
	Verification string                `json:"verification,omitempty"`
	Coverage     string                `json:"coverage"`
	Findings     []reviewResultFinding `json:"findings"`
}

type reviewResultFinding struct {
	ID        string   `json:"id"`
	Member    string   `json:"member"`
	Path      string   `json:"path"`
	StartLine int      `json:"startLine"`
	EndLine   int      `json:"endLine"`
	Severity  string   `json:"severity"`
	Title     string   `json:"title"`
	Evidence  []string `json:"evidence"`
	Detail    string   `json:"detail"`
}

const coveragePrefix = "Review coverage:"

func reviewCompletionInstructions(template string) string {
	if template == "" {
		return ""
	}
	return "\nFinish your final reply with a single line beginning Review coverage: followed by a brief account of what you actually inspected and any gaps. State this even when you found no issues."
}

func reviewCoverage(reply string) string {
	lines := strings.Split(strings.TrimSpace(reply), "\n")
	line := strings.TrimSpace(lines[len(lines)-1])
	if coverage, ok := strings.CutPrefix(line, coveragePrefix); ok {
		return strings.TrimSpace(coverage)
	}
	return ""
}

func (b *Bench) publishReviewResult(workspace string, run reviews.Run, comments []reviews.Comment, coverage, reply string) error {
	if coverage == "" {
		return errors.New("the reviewer did not report coverage")
	}
	if b.Artifacts == nil || len(run.Input) == 0 {
		return errors.New("the artifact store or captured review input is unavailable")
	}
	result := reviewResult{Schema: "lazybox.review-findings/v1", Run: run.ID, Template: run.TemplateID, Input: run.Input, Coverage: coverage, Findings: []reviewResultFinding{}}
	if run.TemplateID == reviews.VerificationTemplate {
		result.Verification = strings.TrimSpace(reply)
	}
	for _, f := range comments {
		if f.ReviewID == run.ID {
			result.Findings = append(result.Findings, reviewResultFinding{ID: f.ID, Member: f.Member, Path: f.Path,
				StartLine: f.StartLine, EndLine: f.EndLine, Severity: f.Severity, Title: f.Title, Evidence: f.Snippet, Detail: f.Body})
		}
	}
	data, err := json.MarshalIndent(result, "", "  ")
	if err != nil {
		return err
	}
	var body strings.Builder
	fmt.Fprintf(&body, "# %s\n\n%d findings filed against the captured changes. \n\n**Coverage** %s\n\n", run.ReviewerTitle, len(result.Findings), coverage)
	if result.Verification != "" {
		fmt.Fprintf(&body, "## Verification report\n\n%s\n\n", result.Verification)
	}
	for _, f := range result.Findings {
		fmt.Fprintf(&body, "- **%s** · %s (%s/%s:%d)\n", f.Severity, f.Title, f.Member, f.Path, f.StartLine)
	}
	fmt.Fprintf(&body, "\n```json\n%s\n```\n", data)
	_, err = b.Artifacts.PublishReview(workspace, run.ID, run.ReviewerTitle, body.String(), result.Schema,
		artifacts.Source{Kind: "agent", SessionID: run.ReviewerSession, ConversationID: run.ReviewerConversation})
	return err
}
