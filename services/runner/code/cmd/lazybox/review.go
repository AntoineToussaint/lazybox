package main

import (
	"fmt"
	"io"
	"strconv"
	"strings"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox review` is how a reviewer agent reports: one finding per call,
// anchored to a file and lines as they are now, with a severity. Lazybox
// starts the reviewer (Changes → Review with…) and briefs it; its findings
// land in the person's Review tray as comments written by that agent. The
// agent's credential may file findings only in the review it is the reviewer
// of, in its own workspace.

const reviewUsage = `Usage:
  lazybox review add --file <repository>/<path> --line N[-M] --severity blocker|major|minor|nit --title "one line" [--body "details" | --body -] [--key KEY]
  lazybox review list
  lazybox review resolve <finding-id> --note "how it was fixed"
  lazybox review reopen <finding-id> --note "what is still wrong"`

func (c *cli) review(args []string) error {
	if c.subcommandHelp(args, reviewUsage) {
		return nil
	}
	switch args[0] {
	case "add":
		return c.reviewAdd(args[1:])
	case "list", "ls":
		return c.reviewList(args[1:])
	case "resolve":
		return c.reviewVerdict(args[1:], "resolved")
	case "reopen":
		return c.reviewVerdict(args[1:], "open")
	}
	fmt.Fprintln(c.stderr, reviewUsage)
	return fail(exitUsage, "unknown review command %q", args[0])
}

// parseLines reads "12" or "12-18" (also "12:18" and "12..18").
func parseLines(text string) (uint32, uint32, error) {
	text = strings.TrimSpace(text)
	for _, sep := range []string{"-", ":", "..", "–"} {
		if a, b, ok := strings.Cut(text, sep); ok {
			start, err1 := strconv.ParseUint(strings.TrimSpace(a), 10, 32)
			end, err2 := strconv.ParseUint(strings.TrimSpace(b), 10, 32)
			if err1 != nil || err2 != nil || start == 0 || end < start {
				return 0, 0, fail(exitUsage, "--line is a line number or a range such as 12-18 (got %q)", text)
			}
			return uint32(start), uint32(end), nil
		}
	}
	n, err := strconv.ParseUint(text, 10, 32)
	if err != nil || n == 0 {
		return 0, 0, fail(exitUsage, "--line is a line number or a range such as 12-18 (got %q)", text)
	}
	return uint32(n), uint32(n), nil
}

// reviewerSession is the execution a reviewer agent runs as.
func (c *cli) reviewerSession() (string, error) {
	session := c.getenv("LAZYBOX_V2_SESSION_ID")
	if session == "" {
		return "", fail(exitUsage, "lazybox review is for a reviewer agent: run it from the review conversation Lazybox started (Changes → Review with…)")
	}
	return session, nil
}

func (c *cli) reviewAdd(args []string) error {
	f := c.flags("review")
	file := f.String("file", "", "the file, as <repository>/<path> from the workspace root")
	line := f.String("line", "", "the line, or a range such as 12-18, as the file is now")
	severity := f.String("severity", "", "blocker, major, minor or nit")
	title := f.String("title", "", "the finding in one line")
	body := f.String("body", "", "what is wrong, why it matters, what to do (Markdown); - reads it from stdin")
	key := f.String("key", "", "a stable key: filing the same key again updates the finding")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) > 0 {
		return fail(exitUsage, "unexpected %q; quote the --title and --body text", strings.Join(positional, " "))
	}
	if *file == "" || *line == "" || *severity == "" || strings.TrimSpace(*title) == "" {
		fmt.Fprintln(c.stderr, reviewUsage)
		return fail(exitUsage, "--file, --line, --severity and --title are required")
	}
	start, end, err := parseLines(*line)
	if err != nil {
		return err
	}
	text := *body
	if text == "-" {
		data, err := io.ReadAll(io.LimitReader(c.stdin, 64<<10+1))
		if err != nil {
			return err
		}
		text = string(data)
	}
	session, err := c.reviewerSession()
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	r, err := got(cl.AddReviewFinding(c.ctx, connect.NewRequest(&pb.AddReviewFindingRequest{SessionId: session, File: *file, StartLine: start, EndLine: end, Severity: *severity, Title: *title, Body: text, Key: *key})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(r)
	}
	finding, verb := r.Finding, "Filed"
	if r.Updated {
		verb = "Updated"
	}
	c.out("%s %s %s at %s/%s:%s · %s\n", verb, finding.Severity, finding.Id, finding.Member, finding.Path, lineRange(finding.StartLine, finding.EndLine), finding.Title)
	return nil
}

func lineRange(start, end uint32) string {
	if start == end {
		return strconv.Itoa(int(start))
	}
	return fmt.Sprintf("%d-%d", start, end)
}

func (c *cli) reviewList(args []string) error {
	f := c.flags("review")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) > 0 {
		return fail(exitUsage, "usage: lazybox review list")
	}
	session, err := c.reviewerSession()
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	r, err := got(cl.ListReviewFindings(c.ctx, connect.NewRequest(&pb.ListReviewFindingsRequest{SessionId: session})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(r)
	}
	run := r.Run
	c.out("%s · %s reviewing %s · %s\n", run.ReviewerTitle, run.ReviewerLabel, run.SourceTitle, run.State)
	if len(r.Findings) == 0 {
		c.out("  No findings filed yet.\n")
		return nil
	}
	for _, finding := range r.Findings {
		where := fmt.Sprintf("%s/%s:%s", finding.Member, finding.Path, lineRange(finding.StartLine, finding.EndLine))
		if finding.Freshness == "moved" {
			where += fmt.Sprintf(" (now %s)", lineRange(finding.CurrentStartLine, finding.CurrentEndLine))
		} else if finding.Freshness == "outdated" {
			where += " (those lines changed since)"
		}
		c.out("  %s\t%-7s %s · %s · %s\n", finding.Id, finding.Severity, where, finding.Title, finding.Disposition)
	}
	return nil
}

// reviewVerdict is the reviewer's resolve or reopen of one of its findings,
// in a later round of the loop.
func (c *cli) reviewVerdict(args []string, state string) error {
	f := c.flags("review")
	note := f.String("note", "", "why: how it was fixed, or what is still wrong")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox review resolve|reopen <finding-id> --note \"why\" (lazybox review list shows the ids)")
	}
	session, err := c.reviewerSession()
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	r, err := got(cl.SetReviewFindingState(c.ctx, connect.NewRequest(&pb.SetReviewFindingStateRequest{SessionId: session, FindingId: positional[0], State: state, Note: *note})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(r)
	}
	word := "Resolved"
	if state == "open" {
		word = "Still open"
	}
	c.out("%s: %s %s · %s\n", word, r.Finding.Id, r.Finding.Severity, r.Finding.Title)
	return nil
}
