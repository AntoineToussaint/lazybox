package reviews

import (
	"errors"
	"strings"
	"testing"
)

func TestParseSeverityRefusesWhatItCannotPlace(t *testing.T) {
	for word, want := range map[string]string{"blocker": SeverityBlocker, " Critical ": SeverityBlocker, "HIGH": SeverityBlocker, "major": SeverityMajor, "medium": SeverityMajor,
		"minor": SeverityMinor, "low": SeverityMinor, "nit": SeverityNit, "nitpick": SeverityNit, "info": SeverityNit} {
		if got, err := ParseSeverity(word); err != nil || got != want {
			t.Errorf("%q: %q %v, want %q", word, got, err, want)
		}
	}
	// A typo is refused, never quietly demoted (V1 review.rs).
	for _, word := range []string{"", "blokker", "urgent", "5"} {
		if _, err := ParseSeverity(word); !errors.Is(err, ErrSeverity) {
			t.Errorf("%q accepted: %v", word, err)
		}
	}
	if SeverityRank(SeverityBlocker) >= SeverityRank(SeverityMajor) || SeverityRank(SeverityNit) >= SeverityRank("") {
		t.Fatal("severities out of order")
	}
}

// `/review <focus>`: the person's focus is in the brief, before how to
// report, or where the template puts {{focus}}; no focus, no section.
func TestBriefCarriesThePersonsFocus(t *testing.T) {
	brief := Brief(BriefInput{Reviewer: "Codex", Scope: "Changes", Focus: "the retry loop in handler.go"})
	focus, report := strings.Index(brief, "## What the person asked you to focus on\n\n> the retry loop in handler.go"), strings.Index(brief, "## How to report")
	if focus < 0 || report < 0 || focus > report {
		t.Fatalf("focus before reporting:\n%s", brief)
	}
	placed := Brief(BriefInput{Scope: "Changes", Focus: "races", Template: "{{focus}}\n\nReview {{scope}}."})
	if !strings.HasPrefix(placed, "## What the person asked you to focus on\n\n> races") || strings.Count(placed, "focus on") != 1 {
		t.Fatalf("the template's own {{focus}}:\n%s", placed)
	}
	if bare := Brief(BriefInput{Scope: "Changes", Template: "{{focus}}\n\nReview {{scope}}."}); strings.Contains(bare, "focus on") || strings.Contains(bare, "{{") {
		t.Fatalf("no focus, no section:\n%s", bare)
	}
}

func TestBriefCarriesTheGoalScopeEvidenceAndAlwaysTheReportingInstructions(t *testing.T) {
	in := BriefInput{Reviewer: "Codex", Author: "Claude", Conversation: "Harden the handler", Goal: "Return errors from Handle instead of logging them.",
		Plan: "“Handler plan” (artifact a1, version 2):\n\n1. Return the error\n2. Test it", Scope: "Turn 3", Diff: "    git -C /w/api diff aaa bbb",
		Files: []string{"api/handler.go (M, +3 −1)"}, Evidence: []string{"`go test ./...` · passed (exit 0)"}}
	brief := Brief(in)
	for _, want := range []string{"independent reviewer (Codex) of work Claude did", "“Harden the handler”", "> Return errors from Handle instead of logging them.",
		"> 1. Return the error", "## What to review: Turn 3", "git -C /w/api diff aaa bbb", "- api/handler.go (M, +3 −1)", "`go test ./...` · passed (exit 0)",
		"lazybox review add --file", "--severity blocker|major|minor|nit", "lazybox review list", "You are read-only"} {
		if !strings.Contains(brief, want) {
			t.Errorf("brief lacks %q:\n%s", want, brief)
		}
	}
	if strings.Contains(brief, "{{") || strings.Contains(brief, "\n\n\n") {
		t.Fatalf("placeholders or blank runs left:\n%s", brief)
	}
	// Nothing recorded: said plainly, not left blank.
	bare := Brief(BriefInput{Reviewer: "Claude", Scope: "Changes"})
	for _, want := range []string{"no recorded first message", "no changed files found", "No `lazybox run` results"} {
		if !strings.Contains(bare, want) {
			t.Errorf("bare brief lacks %q:\n%s", want, bare)
		}
	}
	// The person's template: their words, and the reporting section even when
	// they left {{report}} out (no finding would ever arrive without it).
	custom := Brief(BriefInput{Reviewer: "Codex", Scope: "PR #12", Template: "Be terse. Review {{scope}} for {{author}}."})
	if !strings.HasPrefix(custom, "Be terse. Review PR #12 for another agent.") || !strings.Contains(custom, "lazybox review add") {
		t.Fatalf("custom template:\n%s", custom)
	}
	if strings.Count(Brief(BriefInput{Template: "{{report}}\n\nAnd {{report}}"}), "## How to report") != 2 {
		t.Fatal("a template's own {{report}} placements are kept")
	}
	// A long goal is shortened, and says where the rest is.
	long := Brief(BriefInput{Goal: strings.Repeat("word ", 4000)})
	if !strings.Contains(long, "shortened; the full text is in the conversation") || len(long) > 16<<10 {
		t.Fatalf("long goal not shortened: %d bytes", len(long))
	}
}

// A reviewer's shell is read-only, so the brief sends its tests through the
// Runner's disposable checkout, for every template and every person's
// template, and no longer tells it to run them with `lazybox run`.
func TestBriefSendsTestsThroughTheIsolatedCheckout(t *testing.T) {
	for name, in := range map[string]BriefInput{
		"default":  {Reviewer: "Codex", Scope: "Changes"},
		"custom":   {Reviewer: "Codex", Scope: "PR #12", Template: "Be terse. Review {{scope}}."},
		"verified": {Reviewer: "Codex", Scope: "Changes", Guidance: verificationGuidance},
	} {
		brief := Brief(in)
		if !strings.Contains(brief, "lazybox review test --member <repository>") || !strings.Contains(brief, "disposable, writable checkout") {
			t.Errorf("%s brief does not route tests through the isolated checkout:\n%s", name, brief)
		}
		for _, stale := range []string{"such as the tests, with `lazybox run`", "run the tests with `lazybox run`"} {
			if strings.Contains(brief, stale) {
				t.Errorf("%s brief still sends tests to the read-only shell (%q):\n%s", name, stale, brief)
			}
		}
		// What is left to `lazybox run` is said to write nothing.
		if run := strings.Index(brief, "Use `lazybox run -- <command>`"); run < 0 || !strings.Contains(brief[run:], "only for commands that write nothing") {
			t.Errorf("%s brief leaves `lazybox run` without its limit:\n%s", name, brief)
		}
	}
}

func TestFindingsAreFiledOnceByKeyAndCountedByLiveSeverity(t *testing.T) {
	dir := t.TempDir()
	s, err := New(dir)
	if err != nil {
		t.Fatal(err)
	}
	run, err := s.CreateRun("w1", Run{ReviewerSession: "e-reviewer", ReviewerConversation: "c-reviewer", ReviewerLabel: "Codex", SourceSession: "e-author", SourceConversation: "c-author", Scope: ScopeTurn, ScopeLabel: "Turn 1"})
	if err != nil || run.ID == "" || run.State != RunRunning {
		t.Fatal(run, err)
	}
	finding := func(line int, severity, title, key string) Comment {
		return Comment{Member: "api", Path: "handler.go", StartLine: line, Snippet: []string{"func Handle() {"}, Body: "Why: " + title, Author: AuthorAgent, AuthorLabel: "Codex", AuthorSession: "e-reviewer", Severity: severity, Title: title, ReviewID: run.ID, Key: key}
	}
	first, updated, err := s.AddFinding("w1", finding(3, "critical", "Error  dropped", ""))
	if err != nil || updated || first.Severity != SeverityBlocker || first.Disposition != Proposed || first.Title != "Error dropped" || first.Key == "" || first.EndLine != 3 || first.View != "uncommitted" {
		t.Fatal(first, updated, err)
	}
	// The same file, line and title again: the same finding, updated.
	again, updated, err := s.AddFinding("w1", finding(3, "major", "error dropped", ""))
	if err != nil || !updated || again.ID != first.ID || again.Severity != SeverityMajor || again.Disposition != Proposed {
		t.Fatal(again, updated, err)
	}
	// An explicit key wins over the line: a finding that moved stays one.
	keyed, _, err := s.AddFinding("w1", finding(9, "minor", "Missing test", "missing-test"))
	if err != nil {
		t.Fatal(err)
	}
	moved, updated, err := s.AddFinding("w1", finding(12, "minor", "Missing test for errors", "missing-test"))
	if err != nil || !updated || moved.ID != keyed.ID || moved.StartLine != 12 {
		t.Fatal(moved, updated, err)
	}
	if _, _, err = s.AddFinding("w1", finding(1, "nit", "", "")); err == nil {
		t.Fatal("a finding without a title was accepted")
	}
	if _, _, err = s.AddFinding("w1", finding(1, "whatever", "x", "")); !errors.Is(err, ErrSeverity) {
		t.Fatal("an unknown severity was accepted", err)
	}
	if _, _, err = s.AddFinding("w1", Comment{Member: "api", Path: "a.go", StartLine: 1, Snippet: []string{"x"}, Author: AuthorAgent, Severity: "nit", Title: "x", ReviewID: "rv-missing"}); err == nil {
		t.Fatal("a finding for an unknown review was accepted")
	}
	nit, _, _ := s.AddFinding("w1", finding(20, "nit", "Name", ""))
	review, _ := s.Load("w1")
	if c := CountFindings(review, run.ID); c != (Counts{Majors: 1, Minors: 1, Nits: 1}) || c.Summary() != "1 major, 1 minor, 1 nit" {
		t.Fatalf("counts %+v %q", c, c.Summary())
	}
	// Dismissed (with the person's reason) and resolved findings stop counting.
	if review, err = s.SetDisposition("w1", []string{nit.ID}, Dismissed, Handover{}, "  we keep this name  "); err != nil {
		t.Fatal(err)
	}
	for _, c := range review.Comments {
		if c.ID == nit.ID && (c.Disposition != Dismissed || c.Note != "we keep this name") {
			t.Fatalf("dismissed: %+v", c)
		}
	}
	if _, err = s.SetDisposition("w1", []string{first.ID}, Open, Handover{}); err != nil {
		t.Fatal(err)
	}
	if _, err = s.SetDisposition("w1", []string{first.ID}, Proposed, Handover{}); err == nil {
		t.Fatal("a finding was moved back to proposed by hand")
	}
	review, _ = s.Load("w1")
	if c := CountFindings(review, run.ID); c != (Counts{Majors: 1, Minors: 1}) || len(Findings(review, run.ID)) != 3 {
		t.Fatalf("after triage %+v", c)
	}
	if (Counts{}).Summary() != "no open findings" || (Counts{Blockers: 2, Minors: 3}).Summary() != "2 blockers, 3 minor" {
		t.Fatal("summary wording")
	}

	// A new store (a Runner restart) finds the review from either side.
	s, err = New(dir)
	if err != nil {
		t.Fatal(err)
	}
	if m, ok := s.ReviewerRun("c-reviewer"); !ok || m.Run.ID != run.ID || m.Workspace != "w1" || m.Role != RoleReviewer {
		t.Fatal("reviewer not found after restart", m, ok)
	}
	if matches := s.RunsOf("e-author"); len(matches) != 1 || matches[0].Role != RoleAuthor {
		t.Fatal("author not found", matches)
	}
	if _, ok := s.ReviewerRun("e-author"); ok {
		t.Fatal("the author was taken for the reviewer")
	}
	second, err := s.CreateRun("w1", Run{ReviewerSession: "e-second", ReviewerLabel: "Claude", SourceSession: "e-author", ScopeLabel: "Changes"})
	if err != nil {
		t.Fatal(err)
	}
	if m, ok := s.ReviewerRun("e-second"); !ok || m.Run.ID != second.ID {
		t.Fatal("a review created after the index was built is found", m, ok)
	}
	if updatedRun, err := s.UpdateRun("w1", run.ID, func(r *Run) error { r.State = RunDone; return nil }); err != nil || updatedRun.State != RunDone {
		t.Fatal(updatedRun, err)
	}
}
