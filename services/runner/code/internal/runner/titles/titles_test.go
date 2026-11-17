package titles

import (
	"strings"
	"testing"
	"unicode/utf8"
)

// The prompt from the report that started this: the header showed it cut
// mid-sentence ("…write down a").
const longPrompt = "I want to start a plan, we want to look at SOTA but the goal is to write down a design for conversation titles that never get cut in the middle of a word, and then we build it."

func TestFallbackCutsAtAWordAndDropsFiller(t *testing.T) {
	got := Fallback(longPrompt)
	if got != "Start a plan, we want to look at SOTA but the goal…" {
		t.Fatalf("got %q", got)
	}
	if utf8.RuneCountInString(got) > Limit+1 {
		t.Fatalf("too long: %q", got)
	}
}

func TestFallbackNeverCutsInsideAWord(t *testing.T) {
	for _, text := range []string{longPrompt, strings.Repeat("investigate ", 20), "The login test is flaky on CI and only on WebKit when the network is slow and the retry kicks in late"} {
		got := Fallback(text)
		head := strings.TrimSuffix(got, "…")
		if head == got {
			continue
		}
		words := strings.Fields(head)
		last := strings.ToLower(words[len(words)-1])
		if !strings.Contains(strings.ToLower(text), last+" ") {
			t.Fatalf("%q ends inside a word (%q)", got, last)
		}
	}
}

func TestFallbackCases(t *testing.T) {
	for in, want := range map[string]string{
		"":                                  "",
		"   \n\n  ":                         "",
		"can you fix the flaky login test?": "Fix the flaky login test?",
		"Hey, could you please rename the CLI flags":                                        "Rename the CLI flags",
		"The login test is flaky. Can you find out why it fails on CI and fix it properly?": "The login test is flaky",
		"## Plan\n\nRefactor the runner":                                                    "Plan",
		"```go\nfunc main() {}\n```\nwhy does this panic":                                   "Why does this panic",
		"- add dark mode to settings":                                                       "Add dark mode to settings",
		"let's ship v2.1 today":                                                             "Ship v2.1 today",
		"I'd like to migrate the store to SQLite":                                           "Migrate the store to SQLite",
		"ok":                           "Ok",
		"So the build is broken again": "The build is broken again",
		"Sorting is broken":            "Sorting is broken",
	} {
		if got := Fallback(in); got != want {
			t.Errorf("Fallback(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestFallbackOneHugeWord(t *testing.T) {
	got := Fallback(strings.Repeat("x", 200))
	if utf8.RuneCountInString(got) != Limit || !strings.HasSuffix(got, "…") {
		t.Fatalf("got %q", got)
	}
}

func TestTidy(t *testing.T) {
	for in, want := range map[string]string{
		`"Conversation titles in the header"`:            "Conversation titles in the header",
		"Title: Login flake on WebKit.":                  "Login flake on WebKit",
		"  \n“Runner restart keeps agents”\nsecond line": "Runner restart keeps agents",
		"":          "",
		"  ...  ":   "",
		"`\"\"`":    "",
		"dark mode": "Dark mode",
	} {
		if got := Tidy(in); got != want {
			t.Errorf("Tidy(%q) = %q, want %q", in, got, want)
		}
	}
	long := Tidy(strings.Repeat("word ", 40))
	if utf8.RuneCountInString(long) > generatedLimit+1 || !strings.HasSuffix(long, "…") || strings.Contains(long, "wor…") {
		t.Fatalf("long title: %q", long)
	}
}

func TestDescriptionBoundsBothParts(t *testing.T) {
	d := Description(strings.Repeat("q", 5000), strings.Repeat("z", 5000))
	if strings.Count(d, "q") != 2000 || strings.Count(d, "z") != 600 {
		t.Fatalf("unbounded description: %d", len(d))
	}
	if Description("fix it", "") != "fix it" {
		t.Fatal("a reply that is not there is not mentioned")
	}
}
