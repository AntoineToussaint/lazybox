package reviews

import (
	"fmt"
	"regexp"
	"slices"
	"strings"
)

// The messages the loop sends. FindingsMessage is the very message the tray
// puts in the author's message box (review-model.ts reviewMessage): the same
// findings, the same words, so a finding reaches the author exactly as filed
// whether a person or Lazybox sends it. testdata/findings-message.json pins
// both implementations to one expected text.

var hexCommit = regexp.MustCompile(`^(?i)[0-9a-f]{12,}$`)

func shortCommit(commit string) string {
	if hexCommit.MatchString(commit) {
		return commit[:8]
	}
	return commit
}

func lineRange(start, end int) string {
	if start == end {
		return fmt.Sprint(start)
	}
	return fmt.Sprintf("%d–%d", start, end)
}

func linesWord(start, end int) string {
	if start == end {
		return ""
	}
	return "s"
}

var languages = map[string]string{"ts": "ts", "tsx": "tsx", "js": "js", "jsx": "jsx", "mjs": "js", "go": "go", "rs": "rust", "py": "python", "json": "json", "yaml": "yaml", "yml": "yaml", "sh": "bash", "md": "markdown", "css": "css", "html": "html", "toml": "toml", "sql": "sql", "java": "java", "kt": "kotlin", "swift": "swift", "rb": "ruby", "c": "c", "h": "c", "cpp": "cpp", "proto": "proto"}

func languageOf(path string) string {
	i := strings.LastIndex(path, ".")
	if i < 0 {
		return languages[strings.ToLower(path)]
	}
	return languages[strings.ToLower(path[i+1:])]
}

var backticks = regexp.MustCompile("`+")

func fence(text string) string {
	n := 3
	for _, run := range backticks.FindAllString(text, -1) {
		n = max(n, len(run)+1)
	}
	return strings.Repeat("`", n)
}

// Anchored is a comment with where it points now.
type Anchored struct {
	Comment
	Anchor Anchor
}

func location(c Comment) string { return c.Member + "/" + c.Path }

func where(a Anchored) string {
	c, file, r := a.Comment, location(a.Comment), lineRange(a.StartLine, a.EndLine)
	if c.Side == SideOld {
		return fmt.Sprintf("%s — removed line%s %s (in the version before the change)", file, linesWord(c.StartLine, c.EndLine), r)
	}
	if a.Anchor.Freshness == Moved {
		return fmt.Sprintf("%s:%s (was line%s %s when I commented)", file, lineRange(a.Anchor.StartLine, a.Anchor.EndLine), linesWord(c.StartLine, c.EndLine), r)
	}
	return file + ":" + r
}

func diffLabel(c Comment) string {
	head := "no commit"
	if c.Head != "" {
		head = shortCommit(c.Head)
	}
	switch c.View {
	case "staged":
		return "staged changes on " + head
	case "branch":
		base := "workspace start"
		if c.Base != "" {
			base = shortCommit(c.Base)
		}
		return "branch diff " + base + "…" + head
	}
	return "uncommitted changes on " + head
}

func severityLabel(severity string) string {
	if severity == "" {
		return ""
	}
	return strings.ToUpper(severity[:1]) + severity[1:]
}

// ByFile orders comments as the tray groups them: by member/path, then line,
// then age.
func ByFile(comments []Anchored) []Anchored {
	out := slices.Clone(comments)
	slices.SortStableFunc(out, func(a, b Anchored) int {
		if x, y := strings.ToLower(location(a.Comment)), strings.ToLower(location(b.Comment)); x != y {
			return strings.Compare(x, y)
		}
		if a.StartLine != b.StartLine {
			return a.StartLine - b.StartLine
		}
		return int(a.CreatedAt - b.CreatedAt)
	})
	return out
}

// FindingsMessage is the one message comments and findings become.
func FindingsMessage(comments []Anchored) string {
	ordered := ByFile(comments)
	outdated, labels := false, []string{}
	for _, a := range ordered {
		if a.Anchor.Freshness == Outdated {
			outdated = true
		}
		if a.Author == AuthorAgent && a.AuthorLabel != "" && !slices.Contains(labels, a.AuthorLabel) {
			labels = append(labels, a.AuthorLabel)
		}
	}
	var parts []string
	if len(ordered) == 1 {
		parts = append(parts, "Please address this review comment on the workspace’s changes. It names the file and lines, quotes them as they were when I commented, and says which diff I was reading.")
	} else {
		parts = append(parts, fmt.Sprintf("Please address these %d review comments on the workspace’s changes. Each names the file and lines, quotes them as they were when I commented, and says which diff I was reading.", len(ordered)))
	}
	if outdated {
		parts = append(parts, "Comments marked outdated were made on lines that have changed since; find the same place in the current code, or tell me if it no longer applies.")
	}
	for i, a := range ordered {
		quoted := strings.Join(a.Snippet, "\n")
		f := fence(quoted)
		facts := []string{diffLabel(a.Comment)}
		if a.Anchor.Freshness == Outdated {
			facts = append(facts, "outdated: these lines have changed since")
		}
		section := []string{fmt.Sprintf("### %d. %s", i+1, where(a))}
		if a.Author == AuthorAgent {
			label := severityLabel(a.Severity)
			if label == "" {
				label = "Finding"
			}
			from := "from " + firstNonEmpty(a.AuthorLabel, "the reviewer")
			section = append(section, fmt.Sprintf("**%s: %s** (finding %s, %s)", label, a.Title, a.ID, from))
		}
		section = append(section, "_"+strings.Join(facts, " · ")+"_", f+languageOf(a.Path)+"\n"+quoted+"\n"+f, strings.TrimSpace(a.Body))
		parts = append(parts, strings.Join(section, "\n\n"))
	}
	if len(labels) > 0 {
		parts = append(parts, fmt.Sprintf("The findings are %s’s, reviewing your work. To ask about one, run `lazybox ask reviewer \"<your question>\"`; the reply comes back to you.", strings.Join(labels, " and ")))
	}
	parts = append(parts, "When you are done, say which comments you addressed and how, and which you did not and why.")
	return strings.Join(parts, "\n\n")
}

// RereviewMessage asks the reviewer for round n: only the author's turn,
// against the findings still open, each to resolve or reopen.
func RereviewMessage(run Run, open []Comment, diff []string) string {
	author := firstNonEmpty(run.SourceLabel, "The author")
	turn := ""
	if r := run.current(); r != nil && r.AuthorTurn > 0 {
		turn = fmt.Sprintf(" in turn %d", r.AuthorTurn)
	}
	lines := []string{fmt.Sprintf("[Lazybox] Round %d of %d. %s changed files%s after your findings. Re-review only that change, against your findings still open.", run.Round, run.MaxRounds, author, turn)}
	if len(diff) > 0 {
		lines = append(lines, "Read exactly what changed with:\n\n"+strings.Join(diff, "\n"))
	}
	if len(open) > 0 {
		var items []string
		for _, c := range open {
			items = append(items, fmt.Sprintf("- %s [%s] %s/%s:%s — %s", c.ID, c.Severity, c.Member, c.Path, lineRange(c.StartLine, c.EndLine), c.Title))
		}
		lines = append(lines, "Your findings still open:\n\n"+strings.Join(items, "\n"))
	}
	lines = append(lines, "For each one, run `lazybox review resolve <id> --note \"how it was fixed\"` or `lazybox review reopen <id> --note \"what is still wrong\"`; a finding you leave alone counts as still open. File anything new the change introduced with `lazybox review add`; do not file an open finding again. To ask "+firstNonEmpty(run.SourceLabel, "the author")+" something, run `lazybox ask author \"<question>\"`. End your turn with one line: resolved, still open, new.")
	return strings.Join(lines, "\n\n")
}
