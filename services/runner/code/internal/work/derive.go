package work

import (
	"fmt"
	"iter"
	"strings"
)

// Derived task statuses, ORCHESTRATION.md §2 "Derived state", first match wins.
// verifying is reserved for the verifier role of a later slice.
const (
	StatusCanceled         = "canceled"
	StatusDone             = "done"
	StatusFailed           = "failed"
	StatusNeedsYou         = "needs_you"
	StatusVerifying        = "verifying"
	StatusChangesRequested = "changes_requested"
	StatusWorking          = "working"
	StatusClaimedDone      = "claimed_done"
	StatusBlocked          = "blocked"
	StatusReady            = "ready"
)

// Trust levels, weakest first: an agent or a person said so; the Runner saw
// the exit status or the process; CI on the same commit agrees; a person
// accepted it at a gate.
const (
	TrustClaimed  = "claimed"
	TrustObserved = "observed"
	TrustVerified = "verified"
	TrustAccepted = "accepted"
)

// Flags shown beside a status.
const (
	FlagStale      = "stale"       // CI evidence is pinned to an older head than the latest run or commit
	FlagFailing    = "failing"     // the latest lazybox run failed
	FlagCIFailing  = "ci_failing"  // checks failed on the latest head
	FlagNoEvidence = "no_evidence" // nothing ran and no check reported
	FlagBlockers   = "blockers"    // the latest agent review has open blockers
)

// Liveness of a conversation that is an attempt, as the Runner observes it.
const (
	LiveRunning = "running"
	LiveWaiting = "waiting" // a question or an approval waits for a person
	LiveIdle    = "idle"
	LiveClosed  = "closed"
	LiveMissing = "missing"
)

type Liveness struct {
	State string
	// Session is the conversation's latest execution, to open it.
	Session string
	// Detail is one line: the tool running, the question asked, why it closed.
	Detail string
	// Title is the conversation's name, to show where the work happens.
	Title string
}

// Observations are the live facts a board is derived with besides the log.
type Observations struct {
	Conversations map[string]Liveness
}

// TaskView is one row of a board: the task, its derived status, and what
// backs it.
type TaskView struct {
	*Task
	Status string
	// Trust backs the status; empty where the status claims nothing (ready,
	// blocked, canceled).
	Trust string
	// EvidenceTrust is what the recorded evidence alone supports.
	EvidenceTrust string
	Reason        string
	Flags         []string
	Gate          string
	Attempt       *Attempt
	Live          Liveness
}

// Derive is the board of a folded mission: a pure function of the state and
// the observations, recomputed whenever either changes.
func Derive(s *State, obs Observations) []TaskView {
	views := make([]TaskView, 0, len(s.Tasks))
	done := map[string]bool{}
	for _, t := range s.Tasks {
		done[t.ID] = s.Done(t)
	}
	for _, t := range s.Tasks {
		views = append(views, derive(s, t, obs, done))
	}
	return views
}

func derive(s *State, t *Task, obs Observations, done map[string]bool) TaskView {
	v := TaskView{Task: t, Attempt: t.Current()}
	v.EvidenceTrust, v.Flags, v.Reason = judge(t)
	evidence := v.Reason
	if v.Attempt != nil {
		v.Live = obs.Conversations[v.Attempt.Conversation]
		if v.Live.State == "" {
			v.Live.State = LiveMissing
		}
	}
	report, reported := t.LastReport()
	rejected, wasRejected := t.last(GateRejected)
	gate, gateOpen := t.OpenGate()
	live := v.Attempt != nil && !v.Attempt.Ended
	switch {
	case t.Canceled || s.Canceled:
		v.Status, v.Trust = StatusCanceled, ""
		v.Reason = "Canceled" + suffix(firstNonEmpty(t.CancelReason, s.Reason))
	case done[t.ID]:
		passed, _ := t.last(GatePassed)
		v.Status, v.Trust = StatusDone, TrustAccepted
		v.Reason = "Accepted" + suffix(passed.Note) + "; evidence: " + evidence
	case v.Attempt != nil && v.Attempt.Ended && v.Attempt.Outcome != "handoff" && (!reported || report.Seq < v.Attempt.StartSeq):
		v.Status, v.Trust = StatusFailed, TrustObserved
		v.Reason = fmt.Sprintf("Its conversation ended (%s) without a report", firstNonEmpty(v.Attempt.Outcome, "stopped"))
	case live && v.Attempt.Dispatch != nil && v.Attempt.Started.IsZero():
		v.Status, v.Trust = StatusWorking, TrustObserved
		v.Reason = "Workspace and conversation reserved; waiting for task brief delivery"
	case gateOpen:
		v.Status, v.Trust, v.Gate = StatusNeedsYou, v.EvidenceTrust, gate.Gate
		v.Reason = "Reported done, waiting for you to accept · " + evidence
	case live && v.Live.State == LiveWaiting:
		v.Status, v.Trust = StatusNeedsYou, TrustObserved
		v.Reason = "The agent is waiting for you" + suffix(v.Live.Detail)
	case wasRejected && (!reported || rejected.Seq > report.Seq):
		v.Status, v.Trust = StatusChangesRequested, v.EvidenceTrust
		v.Reason = "You asked for changes" + suffix(rejected.Note)
	case live && (v.Live.State == LiveRunning || v.Live.State == LiveIdle):
		v.Status, v.Trust = StatusWorking, TrustObserved
		if v.Live.State == LiveRunning {
			v.Reason = "The agent is working" + suffix(v.Live.Detail)
		} else {
			v.Reason = "The agent finished its turn; reply in the conversation, or report the task done"
		}
	case reported && (!v.hasAttemptAfter(report.Seq)):
		v.Status, v.Trust = StatusClaimedDone, v.EvidenceTrust
		v.Reason = "Reported done · " + evidence
	case blockedBy(t, done) != "":
		v.Status = StatusBlocked
		v.Reason = "Waiting for " + blockedBy(t, done)
		if before := s.tasks[blockedBy(t, done)]; before != nil {
			v.Reason = "Waiting for “" + before.Title + "”"
		}
	default:
		v.Status = StatusReady
		switch {
		case v.Attempt == nil:
			v.Reason = "Not assigned yet"
		case live:
			v.Reason = "Its conversation is not running; open it to continue, or assign the task again"
		default:
			v.Reason = "Its last attempt ended; assign the task again"
		}
	}
	if !v.claims() {
		// "No evidence" is news only beside a claim of done.
		v.Flags = without(v.Flags, FlagNoEvidence)
	}
	return v
}

// claims reports whether the row says the work is done (or was, until a
// person asked for changes): only then does its evidence back the status.
func (v TaskView) claims() bool {
	switch v.Status {
	case StatusDone, StatusClaimedDone, StatusChangesRequested:
		return true
	case StatusNeedsYou:
		return v.Gate != ""
	}
	return false
}

func without(list []string, drop string) []string {
	var out []string
	for _, v := range list {
		if v != drop {
			out = append(out, v)
		}
	}
	return out
}

func (v TaskView) hasAttemptAfter(seq uint64) bool {
	return v.Attempt != nil && v.Attempt.StartSeq > seq && !v.Attempt.Ended
}

func blockedBy(t *Task, done map[string]bool) string {
	for _, before := range t.BlockedBy {
		if !done[before] {
			return before
		}
	}
	return ""
}

// currentEvidence walks the current attempt's journal in order. Older journals can
// contain workspace-wide CI. Only checks after delivery of that PR can
// speak for the attempt. All records remain available in its history.
func (t *Task) currentEvidence() iter.Seq[*Recorded] {
	attemptID := ""
	if attempt := t.Current(); attempt != nil {
		attemptID = attempt.ID
	}
	return func(yield func(*Recorded) bool) {
		delivered := map[string]bool{}
		for i := range t.Evidence {
			r := &t.Evidence[i]
			if r.Attempt != attemptID {
				continue
			}
			if r.Kind == EvidencePR {
				if r.IsPullRequestLink() {
					delivered[r.Ref] = true
				} else if !delivered[PullRequestLinkRef(r.Repository, r.Number)] {
					continue
				}
			}
			if !yield(r) {
				return
			}
		}
	}
}

// CurrentEvidenceSeqs identifies journal entries the current attempt owns,
// including delivery markers shown before their first check arrives.
func (t *Task) CurrentEvidenceSeqs() map[uint64]bool {
	current := make(map[uint64]bool)
	for r := range t.currentEvidence() {
		current[r.Seq] = true
	}
	return current
}

// judge is what the task's evidence supports, with flags and one sentence.
// verified: checks passed on a pull request whose head is the newest the
// Runner saw for that repository. observed: the latest `lazybox run` (with a
// real exit status) passed. Otherwise only a claim.
func judge(t *Task) (trust string, flags []string, reason string) {
	if len(t.Evidence) == 0 {
		return TrustClaimed, []string{FlagNoEvidence}, "no run or check recorded"
	}
	newest := map[string]string{} // member → latest locally observed run or commit revision
	var run, review, boundReview *Recorded
	prs := map[string]*Recorded{} // delivery reference → latest check observation
	order := []string{}
	for r := range t.currentEvidence() {
		switch r.Kind {
		case EvidenceRun:
			for member, head := range r.Revisions {
				newest[member] = head
			}
			if !r.Piped && r.ExitCode != nil {
				run = r
			}
		case EvidenceCommit:
			if r.Member != "" && r.Head != "" {
				newest[r.Member] = r.Head
			}
		case EvidenceReview:
			review = r
			if r.ContractDigest != "" {
				boundReview = r
			}
		case EvidencePR:
			if r.IsPullRequestLink() {
				continue
			}
			key := PullRequestLinkRef(r.Repository, r.Number)
			if prs[key] == nil {
				order = append(order, key)
			}
			prs[key] = r
		}
	}
	var verified, ciFailing, stale, unchecked *Recorded
	for _, key := range order {
		pr := prs[key]
		current := newest[pr.Member] == "" || pr.Head == "" || newest[pr.Member] == pr.Head
		switch {
		case pr.Outcome == OutcomePass && current:
			verified = pr
		case pr.Outcome == OutcomePass:
			stale = pr
		case pr.Outcome == OutcomeFail:
			ciFailing = pr
		default:
			unchecked = pr
		}
	}
	if stale != nil {
		flags = append(flags, FlagStale)
	}
	if ciFailing != nil {
		flags = append(flags, FlagCIFailing)
	}
	if run != nil && run.Outcome == OutcomeFail {
		flags = append(flags, FlagFailing)
	}
	// Open blockers from the latest agent review keep the work from verified:
	// checks passing on code a reviewer found wrong do not vouch for it.
	if boundReview != nil {
		// A passing turn or PR review cannot settle blockers on the pinned candidate.
		if boundReview.Outcome == OutcomeFail {
			review = boundReview
		}
		if reviewStale(t, boundReview) {
			return TrustObserved, appendOnce(flags, FlagStale), "workspace revisions changed or could not be verified; review the current candidate"
		}
		if boundReview.Outcome == OutcomeUnknown || boundReview.Outcome == OutcomePending {
			return TrustObserved, flags, "waiting for a completed review of the current candidate"
		}
	}
	blocked := review != nil && review.Outcome == OutcomeFail
	incomplete := review != nil && review.State == "verification" && review.Outcome != OutcomePass
	if blocked {
		flags = append(flags, FlagBlockers)
	}
	// One delivered PR cannot clear another PR's outstanding checks.
	var checkIssue string
	switch {
	case ciFailing != nil:
		checkIssue = "checks failed on " + prName(ciFailing)
	case stale != nil:
		checkIssue = "checks passed on an older head of " + prName(stale)
	case unchecked != nil:
		checkIssue = "waiting for passing checks on " + prName(unchecked)
	}
	switch {
	case verified != nil && checkIssue == "" && !blocked && !incomplete:
		return TrustVerified, flags, "checks passed on " + prName(verified)
	case run != nil && run.Outcome == OutcomePass:
		reason = "last run passed: " + runName(run)
		if checkIssue != "" {
			reason += "; " + checkIssue
		}
		if blocked || incomplete {
			reason += "; " + reviewName(review)
		}
		return TrustObserved, flags, reason
	case checkIssue != "":
		return TrustClaimed, flags, checkIssue
	case verified != nil && (blocked || incomplete):
		return TrustObserved, flags, "checks passed on " + prName(verified) + "; " + reviewName(review)
	case run != nil:
		return TrustClaimed, flags, "last run failed: " + runName(run)
	}
	return TrustClaimed, append(flags, FlagNoEvidence), "no passing run or check recorded"
}

func reviewStale(t *Task, review *Recorded) bool {
	if review.State == "stale" {
		return true
	}
	for later := range t.currentEvidence() {
		if later.Seq <= review.Seq || later.IsPullRequestLink() {
			continue
		}
		if later.Kind == EvidenceCommit || later.Kind == EvidencePR {
			if h, ok := review.Revisions[later.Member]; ok && later.Head != "" && later.Head != h {
				return true
			}
		}
		if later.Kind == EvidenceRun {
			for member, h := range later.Revisions {
				if before, ok := review.Revisions[member]; ok && before != h {
					return true
				}
			}
		}
	}
	return false
}

func reviewName(r *Recorded) string {
	if r.State == "verification" && r.Outcome != OutcomePass && r.Outcome != OutcomeFail {
		return "acceptance verification is incomplete"
	}
	return firstNonEmpty(r.Title, "an agent review") + " has open blockers" + suffix(r.Detail)
}

func runName(r *Recorded) string {
	code := "?"
	if r.ExitCode != nil {
		code = fmt.Sprint(*r.ExitCode)
	}
	return fmt.Sprintf("%s (exit %s)", firstNonEmpty(r.Title, "command"), code)
}

func prName(r *Recorded) string {
	name := firstNonEmpty(r.Repository, r.Member)
	if r.Number > 0 {
		name += fmt.Sprintf("#%d", r.Number)
	}
	if r.Head != "" {
		name += " at " + short(r.Head)
	}
	return name
}

func short(sha string) string {
	if len(sha) > 7 {
		return sha[:7]
	}
	return sha
}

func suffix(text string) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	return ": " + text
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

// Awaiting reports whether a row waits for a person's decision at a gate:
// the inbox's "awaiting acceptance".
func (v TaskView) Awaiting() bool { return v.Status == StatusNeedsYou && v.Gate != "" }
