package main

import (
	"bytes"
	"context"
	"strings"
	"testing"
)

func TestReviewLinesAcceptARangeInTheFormsAgentsWrite(t *testing.T) {
	for text, want := range map[string][2]uint32{"12": {12, 12}, " 12-18 ": {12, 18}, "12:18": {12, 18}, "12..18": {12, 18}, "12–18": {12, 18}} {
		start, end, err := parseLines(text)
		if err != nil || start != want[0] || end != want[1] {
			t.Errorf("%q: %d-%d %v", text, start, end, err)
		}
	}
	for _, bad := range []string{"", "0", "x", "18-12", "1-", "-3"} {
		if _, _, err := parseLines(bad); exitCode(err) != exitUsage {
			t.Errorf("%q accepted: %v", bad, err)
		}
	}
}

// review add says what is missing before it dials anything, and refuses to
// run outside an agent conversation.
func TestReviewAddExplainsWhatIsMissing(t *testing.T) {
	var out, errs bytes.Buffer
	c := &cli{ctx: context.Background(), stdout: &out, stderr: &errs, getenv: func(string) string { return "" }}
	if err := c.review([]string{"add", "--file", "api/a.go", "--line", "3"}); exitCode(err) != exitUsage || !strings.Contains(err.Error(), "--severity and --title are required") {
		t.Fatalf("missing flags: %v", err)
	}
	err := c.review([]string{"add", "--file", "api/a.go", "--line", "3", "--severity", "nit", "--title", "x"})
	if exitCode(err) != exitUsage || !strings.Contains(err.Error(), "review conversation Lazybox started") {
		t.Fatalf("outside an agent: %v", err)
	}
	if err = c.review([]string{"bogus"}); exitCode(err) != exitUsage {
		t.Fatalf("unknown subcommand: %v", err)
	}
}
