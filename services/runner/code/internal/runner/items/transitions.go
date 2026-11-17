package items

import (
	"strconv"
	"strings"
)

// The events a pull request emits (automate.md's hooks table). Each is a
// transition: a poll that finds nothing new emits nothing.
const (
	EventOpened           = "pr.opened"
	EventChecksFailed     = "pr.checks-failed"
	EventChecksPassed     = "pr.checks-passed"
	EventChangesRequested = "pr.changes-requested"
	EventApproved         = "pr.approved"
	EventMerged           = "pr.merged"
	EventClosed           = "pr.closed"
	EventConflicts        = "pr.conflicts"
)

// Events lists them in the order Settings shows them.
var Events = []string{EventOpened, EventChecksFailed, EventChecksPassed, EventChangesRequested, EventApproved, EventMerged, EventClosed, EventConflicts}

// Transitions are the events between two snapshots of one pull request.
// before is nil the first time Lazybox reads it: that read is a baseline,
// except that a pull request first seen open is opened, and one first seen
// already needing the person (failing checks, changes requested, conflicts)
// says so once, so a Runner upgrade or a late link does not hide it.
func Transitions(before *Item, after Item) []Event {
	if after.Kind != KindPullRequest || after.PR == nil {
		return nil
	}
	event := func(name, reason string) Event {
		return Event{Name: name, Ref: after.Ref, Reason: reason, HeadSHA: after.PR.HeadSHA, Title: after.Title, URL: after.URL, Repository: after.Repository, Number: after.Number}
	}
	var out []Event
	open := after.RawState == "open"
	if before == nil || before.PR == nil {
		if !open {
			return nil
		}
		out = append(out, event(EventOpened, "opened"))
		// The baseline: what the first read shows, except what needs the person.
		baseline := &PullRequest{HeadSHA: after.PR.HeadSHA, Checks: after.PR.Checks, Review: after.PR.Review}
		if baseline.Checks == "failure" {
			baseline.Checks = ""
		}
		if baseline.Review == "changes_requested" {
			baseline.Review = ""
		}
		before = &Item{RawState: "open", PR: baseline}
	}
	old, now := before.PR, after.PR
	switch {
	case before.RawState == "open" && after.RawState == "merged":
		return append(out, event(EventMerged, "merged"))
	case before.RawState == "open" && after.RawState == "closed":
		return append(out, event(EventClosed, "closed without merging"))
	case before.RawState != "open" && open:
		out = append(out, event(EventOpened, "reopened"))
	}
	if !open {
		return out
	}
	newHead := old.HeadSHA != now.HeadSHA
	if now.Checks == "failure" && (old.Checks != "failure" || newHead) {
		ev := event(EventChecksFailed, checksReason(now))
		ev.Failed = now.FailingNames()
		out = append(out, ev)
	}
	if now.Checks == "success" && (old.Checks == "pending" || old.Checks == "failure" || (newHead && old.Checks != "success")) {
		out = append(out, event(EventChecksPassed, "checks passed"))
	}
	if now.Review == "changes_requested" && old.Review != "changes_requested" {
		out = append(out, event(EventChangesRequested, "changes requested"))
	}
	if now.Review == "approved" && old.Review != "approved" {
		out = append(out, event(EventApproved, "approved"))
	}
	if now.MergeableState == "dirty" && old.MergeableState != "dirty" {
		out = append(out, event(EventConflicts, "conflicts with "+firstNonEmpty(now.Base, "its base")))
	}
	return out
}

// checksReason is "checks failed: lint, unit-api (2 of 14)".
func checksReason(pr *PullRequest) string {
	names := pr.FailingNames()
	if len(names) == 0 {
		return "checks failed"
	}
	shown := names
	if len(shown) > 4 {
		shown = shown[:4]
	}
	text := "checks failed: " + strings.Join(shown, ", ")
	if len(names) > len(shown) {
		text += ", …"
	}
	return text + " (" + strconv.Itoa(len(names)) + " of " + strconv.Itoa(len(pr.CheckRuns)) + ")"
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}
