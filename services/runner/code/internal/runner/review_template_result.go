package runner

import (
	"encoding/json"
	"errors"
	"fmt"
	"strconv"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
)

type reviewResult struct {
	Acceptance   string                      `json:"acceptance,omitempty"`
	Checks       []reviews.VerificationCheck `json:"checks,omitempty"`
	Schema       string                      `json:"schema"`
	Run          string                      `json:"run"`
	Template     string                      `json:"template"`
	Input        []reviews.ReviewInput       `json:"input"`
	Verification string                      `json:"verification,omitempty"`
	Coverage     string                      `json:"coverage"`
	Findings     []reviewResultFinding       `json:"findings"`
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
	return b.publishReviewVersion(workspace, run, comments, coverage, reply, fmt.Sprintf("review-round-%d", run.Round))
}

func (b *Bench) publishReviewVersion(workspace string, run reviews.Run, comments []reviews.Comment, coverage, reply, version string) error {
	if coverage == "" {
		return errors.New("the reviewer did not report coverage")
	}
	if b.Artifacts == nil || len(run.Input) == 0 {
		return errors.New("the artifact store or captured review input is unavailable")
	}
	result := reviewResult{Schema: "lazybox.review-findings/v1", Run: run.ID, Template: run.TemplateID, Input: run.Input, Coverage: coverage, Findings: []reviewResultFinding{}}
	if run.TemplateID == reviews.VerificationTemplate {
		result.Acceptance = "unverified"
		if run.StopReason == reviews.StopClean && reviews.VerificationComplete(run) {
			result.Acceptance = "passed"
		}
		result.Verification = strings.TrimSpace(reply)
		result.Checks = run.Verification
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
	if result.Acceptance != "" {
		fmt.Fprintf(&body, "**Acceptance:** %s\n\n", result.Acceptance)
	}
	if len(result.Checks) > 0 {
		body.WriteString("## Recorded acceptance checks\n\n| Criterion | Expected behavior | Round | State | Base exit | Candidate exit | Command output IDs |\n| --- | --- | --- | --- | --- | --- | --- |\n")
		for _, check := range result.Checks {
			for _, receipt := range check.Runs {
				fmt.Fprintf(&body, "| %s | %s | %d | %s | %v | %v | %s, %s |\n", verificationCell(check.Name), verificationCell(check.Expected), receipt.Round, receipt.State, verificationExit(receipt.BaseExit), verificationExit(receipt.CandidateExit), receipt.BaseCommand, receipt.CandidateCommand)
			}
		}
	}
	if result.Verification != "" {
		fmt.Fprintf(&body, "## Verification report\n\n%s\n\n", result.Verification)
	}
	for _, f := range result.Findings {
		fmt.Fprintf(&body, "- **%s** · %s (%s/%s:%d)\n", f.Severity, f.Title, f.Member, f.Path, f.StartLine)
	}
	fmt.Fprintf(&body, "\n```json\n%s\n```\n", data)
	_, err = b.Artifacts.PublishReview(workspace, run.ID, run.ReviewerTitle, body.String(), result.Schema,
		artifacts.Source{Kind: "agent", SessionID: run.ReviewerSession, ConversationID: run.ReviewerConversation, TurnID: version})
	return err
}

func verificationExit(code *int) string {
	if code == nil {
		return "unverified"
	}
	return strconv.Itoa(*code)
}

func verificationCell(text string) string {
	return strings.NewReplacer("|", "\\|", "\n", " ", "\r", " ").Replace(text)
}
