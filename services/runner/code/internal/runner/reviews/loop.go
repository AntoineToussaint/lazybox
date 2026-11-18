package reviews

import (
	"errors"
	"fmt"
	"strings"
)

// The review loop (GITHUB.md, "The review loop"): after the reviewer's pass,
// its findings reach the author (the person sends them, or Lazybox does when
// the loop is "auto"); when the author's turn then ends with changes, the
// reviewer re-reviews only that turn against its open findings, resolving or
// reopening each and filing what is new. It repeats until no blocker is open,
// a round limit, or a blocker still open after two fixes (stuck: the person
// decides). Every step is recorded on the review, which is persisted, so a
// loop outlives a Runner restart.
//
// This file is the loop's pure half: each function changes one review in
// memory and says what the Runner must do (send findings, start a re-review,
// tell the person); the Runner does it (review_loop.go).

// Loop modes.
const (
	LoopAsk  = "ask"
	LoopAuto = "auto"
	LoopOff  = "off"
)

// Phases of a review.
const (
	PhaseReviewing = "reviewing"
	PhaseTriage    = "triage"
	PhaseFixing    = "fixing"
	PhaseDone      = "done"
)

// Why a loop ended.
const (
	StopClean     = "clean"
	StopMaxRounds = "max_rounds"
	StopStuck     = "stuck"
	StopError     = "error"
	StopStopped   = "stopped"
	StopTakenOver = "taken_over"
	// StopCancelled: the person cancelled the review (CancelReview): the
	// reviewer's turn interrupted, its session ended, its findings kept.
	StopCancelled = "cancelled"
	// StopEnded: the person ended the reviewer by hand (Stop, End session,
	// Delete) while the review was under way.
	StopEnded = "ended"
)

// DefaultMaxRounds bounds a loop when nobody said otherwise.
const DefaultMaxRounds = 3

const maxRoundsLimit = 10

// Round is one pass of the reviewer.
type Round struct {
	N          int    `json:"n"`
	Trigger    string `json:"trigger"`
	AuthorTurn int    `json:"authorTurn,omitempty"`
	StartedAt  int64  `json:"startedAt"`
	EndedAt    int64  `json:"endedAt,omitempty"`
	Filed      int    `json:"filed,omitempty"`
	Resolved   int    `json:"resolved,omitempty"`
	StillOpen  int    `json:"stillOpen,omitempty"`
	Blockers   int    `json:"blockers,omitempty"`
	// Sent: the round's message (the re-review) was delivered.
	Sent bool `json:"sent,omitempty"`
}

// Entry is one line of a review's timeline.
type Entry struct {
	At      int64  `json:"at"`
	Kind    string `json:"kind"`
	Actor   string `json:"actor"`
	Text    string `json:"text"`
	Finding string `json:"finding,omitempty"`
	Round   int    `json:"round,omitempty"`
}

const timelineLimit = 400

// Action is what the Runner must do after a step of the loop.
type Action struct {
	// SendFindings: deliver these findings to the author (auto mode).
	SendFindings []string
	// Rereview: send the reviewer the re-review of the current round.
	Rereview bool
	// Escalate: tell the person, in this sentence.
	Escalate string
}

// LoopMode reads a mode word; "" is ask.
func LoopMode(mode string) (string, error) {
	switch strings.ToLower(strings.TrimSpace(mode)) {
	case "", LoopAsk:
		return LoopAsk, nil
	case LoopAuto:
		return LoopAuto, nil
	case LoopOff:
		return LoopOff, nil
	}
	return "", fmt.Errorf("the loop is ask, auto or off (got %q)", mode)
}

// ClampRounds keeps a round limit between 1 and 10; 0 is the default.
func ClampRounds(n int) int {
	if n <= 0 {
		return DefaultMaxRounds
	}
	return min(n, maxRoundsLimit)
}

func (r *Run) note(now int64, kind, actor, text, finding string) {
	r.Timeline = append(r.Timeline, Entry{At: now, Kind: kind, Actor: actor, Text: text, Finding: finding, Round: r.Round})
	if over := len(r.Timeline) - timelineLimit; over > 0 {
		r.Timeline = append(r.Timeline[:0:0], r.Timeline[over:]...)
	}
}

// Active reports whether the loop still runs.
func (r Run) Active() bool { return r.Phase != "" && r.Phase != PhaseDone && r.StopReason == "" }

func (r *Run) current() *Round {
	if len(r.Rounds) == 0 {
		return nil
	}
	return &r.Rounds[len(r.Rounds)-1]
}

// StartLoop sets a new review's loop and its first round.
func StartLoop(r *Run, mode string, maxRounds int, now int64) {
	r.Loop, r.MaxRounds, r.Round, r.Phase = mode, ClampRounds(maxRounds), 1, PhaseReviewing
	r.Rounds = []Round{{N: 1, Trigger: "start", StartedAt: now, Sent: true}}
	what := "one pass"
	switch mode {
	case LoopAsk:
		what = fmt.Sprintf("up to %d rounds; you send findings between rounds", r.MaxRounds)
	case LoopAuto:
		what = fmt.Sprintf("up to %d rounds; findings go to %s automatically", r.MaxRounds, firstNonEmpty(r.SourceLabel, "the author"))
	}
	r.note(now, "round_started", "Lazybox", fmt.Sprintf("Round 1: %s reviews %s (%s).", r.ReviewerLabel, lowerScope(r.ScopeLabel), what), "")
}

func lowerScope(label string) string {
	if label == "" || strings.HasPrefix(label, "PR ") {
		return label
	}
	return strings.ToLower(label[:1]) + label[1:]
}

func liveBlocker(c Comment) bool { return c.Live() && c.Severity == SeverityBlocker }

// RoundEnded closes the reviewer's current round when its turn ends: what it
// filed, resolved and left open, which findings are still open from earlier
// rounds (silence is not a disposition: a finding the reviewer neither
// resolved nor reopened counts as still open), and what comes next.
func RoundEnded(rv *Review, runID string, now int64) (Action, error) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Action{}, err
	}
	if r.Phase != PhaseReviewing {
		return Action{}, nil // a later turn of the reviewer: the person talks to it
	}
	round := r.current()
	if round == nil {
		StartLoop(r, LoopOff, 1, now)
		round = r.current()
	}
	round.EndedAt = now
	round.Filed, round.Resolved, round.StillOpen, round.Blockers = 0, 0, 0, 0
	for i := range rv.Comments {
		c := &rv.Comments[i]
		if c.ReviewID != r.ID {
			continue
		}
		switch {
		case c.Round == r.Round:
			round.Filed++
		case c.Round < r.Round && c.Live() && c.Delivered:
			c.StillOpenRounds++
			round.StillOpen++
		}
		if liveBlocker(*c) {
			round.Blockers++
		}
	}
	for _, e := range r.Timeline {
		if e.Round == r.Round && e.Kind == "resolved" {
			round.Resolved++
		}
	}
	summary := fmt.Sprintf("Round %d ended: %d filed, %d resolved, %d still open; %s open.", r.Round, round.Filed, round.Resolved, round.StillOpen, plural(round.Blockers, "blocker", "blockers"))
	if r.Round == 1 {
		summary = fmt.Sprintf("Round 1 ended: %s; %s open.", plural(round.Filed, "finding", "findings"), plural(round.Blockers, "blocker", "blockers"))
	}
	r.note(now, "round_ended", r.ReviewerLabel, summary, "")
	if r.StopReason != "" {
		r.Phase = PhaseDone
		return Action{}, nil
	}
	var stuck *Comment
	for i := range rv.Comments {
		c := &rv.Comments[i]
		if c.ReviewID == r.ID && liveBlocker(*c) && c.StillOpenRounds >= 2 {
			stuck = c
			break
		}
	}
	switch {
	case stuck != nil:
		why := fmt.Sprintf("The blocker “%s” (%s) is still open after two fixes: decide how to proceed.", stuck.Title, stuck.ID)
		r.stop(now, StopStuck, why)
		return Action{Escalate: why}, nil
	case round.Blockers == 0:
		if r.Loop == LoopOff {
			r.Phase = PhaseDone
			return Action{}, nil
		}
		// The loop iterates on blockers only, so it ends here with majors
		// possibly still open: "clean" is said only when none is.
		majors := 0
		for _, c := range rv.Comments {
			if c.ReviewID == r.ID && c.Live() && c.Severity == SeverityMajor {
				majors++
			}
		}
		why := fmt.Sprintf("Clean: no blocker open after %s.", plural(r.Round, "round", "rounds"))
		if majors > 0 {
			why = fmt.Sprintf("No blocker open after %s; %d major still open.", plural(r.Round, "round", "rounds"), majors)
		}
		r.stop(now, StopClean, why)
		return Action{}, nil
	case r.Loop == LoopOff:
		r.Phase = PhaseDone
		return Action{}, nil
	case r.Round >= r.MaxRounds:
		why := fmt.Sprintf("%s still open after %s, the limit: decide how to proceed.", plural(round.Blockers, "blocker", "blockers"), plural(r.Round, "round", "rounds"))
		r.stop(now, StopMaxRounds, why)
		return Action{Escalate: why}, nil
	case r.Loop == LoopAuto && !r.Paused:
		var ids []string
		for _, c := range rv.Comments {
			if c.ReviewID == r.ID && (c.Disposition == Proposed || c.Disposition == Open) {
				ids = append(ids, c.ID)
			}
		}
		r.Phase = PhaseTriage
		if len(ids) == 0 {
			return Action{}, nil
		}
		return Action{SendFindings: ids}, nil
	}
	// Findings the person already sent while the reviewer worked: the loop
	// waits for the author's fix, from when they were sent.
	undelivered, since := false, int64(0)
	for _, c := range rv.Comments {
		if c.ReviewID != r.ID || !c.Live() {
			continue
		}
		if c.Disposition == Proposed || c.Disposition == Open {
			undelivered = true
		}
		if c.Disposition == Sent && c.SentAt > 0 && (since == 0 || c.SentAt < since) {
			since = c.SentAt
		}
	}
	if !undelivered && since > 0 {
		r.Phase, r.FixingSince = PhaseFixing, since
		r.note(now, "fixing", "Lazybox", fmt.Sprintf("The findings were sent; waiting for %s to change files, then %s re-reviews.", firstNonEmpty(r.SourceLabel, "the author"), r.ReviewerLabel), "")
		return Action{}, nil
	}
	r.Phase = PhaseTriage
	r.note(now, "triage", "Lazybox", "Waiting for you: accept the findings to send, dismiss the others.", "")
	return Action{}, nil
}

func severityWord(severity string) string {
	if severity == "" {
		return "Finding"
	}
	return strings.ToUpper(severity[:1]) + severity[1:]
}

func plural(n int, one, many string) string {
	if n == 1 {
		return "1 " + one
	}
	return fmt.Sprintf("%d %s", n, many)
}

func (r *Run) stop(now int64, reason, text string) {
	r.StopReason, r.Phase = reason, PhaseDone
	r.Detail = text
	kind := "stopped"
	if reason == StopStuck || reason == StopMaxRounds || reason == StopError {
		kind = "escalated"
	}
	r.note(now, kind, "Lazybox", text, "")
}

// FindingsSent records findings handed to the author (by the person through
// the tray, or by Lazybox in auto mode): the loop now waits for the author's
// turn to change files.
func FindingsSent(rv *Review, runID string, ids []string, by, to string, now int64) {
	r, err := findRun(rv, runID)
	if err != nil {
		return
	}
	mine := 0
	for i := range rv.Comments {
		if c := &rv.Comments[i]; c.ReviewID == r.ID && containsID(ids, c.ID) {
			c.Delivered = true
			mine++
		}
	}
	if mine == 0 {
		return
	}
	r.note(now, "sent", by, fmt.Sprintf("Sent %s to %s.", plural(mine, "finding", "findings"), firstNonEmpty(to, r.SourceLabel, "the author")), "")
	if r.Active() && r.Loop != LoopOff && r.Phase != PhaseReviewing {
		r.Phase, r.FixingSince = PhaseFixing, now
		r.note(now, "fixing", "Lazybox", fmt.Sprintf("Waiting for %s to change files; then %s re-reviews.", firstNonEmpty(r.SourceLabel, "the author"), r.ReviewerLabel), "")
	}
}

func containsID(ids []string, id string) bool {
	for _, x := range ids {
		if x == id {
			return true
		}
	}
	return false
}

// AuthorTurnEnded follows the author's turn: one that began after the
// findings were sent and changed files starts the next round.
func AuthorTurnEnded(rv *Review, runID string, turn int, startedAt int64, files int, now int64) (Action, error) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Action{}, err
	}
	if !r.Active() || r.Phase != PhaseFixing || startedAt < r.FixingSince {
		return Action{}, nil
	}
	author := firstNonEmpty(r.SourceLabel, "The author")
	if files == 0 {
		r.note(now, "fixing", author, fmt.Sprintf("Turn %d ended without changing files; still waiting.", turn), "")
		return Action{}, nil
	}
	if r.Paused {
		r.note(now, "fixing", author, fmt.Sprintf("Turn %d changed %s; the loop is paused, so no re-review.", turn, plural(files, "file", "files")), "")
		return Action{}, nil
	}
	r.Round++
	r.Rounds = append(r.Rounds, Round{N: r.Round, Trigger: "author_turn", AuthorTurn: turn, StartedAt: now})
	r.Phase, r.State = PhaseReviewing, RunRunning
	r.note(now, "round_started", "Lazybox", fmt.Sprintf("Round %d: %s changed %s in turn %d; %s re-reviews that turn against its open findings.", r.Round, author, plural(files, "file", "files"), turn, r.ReviewerLabel), "")
	return Action{Rereview: true}, nil
}

// Controls the person has over a loop.
const (
	ControlPause    = "pause"
	ControlResume   = "resume"
	ControlStop     = "stop"
	ControlTakeOver = "take_over"
	ControlLoop     = "loop"
)

// Control applies the person's choice to a review's loop.
func Control(rv *Review, runID, action, mode string, maxRounds int, now int64) error {
	r, err := findRun(rv, runID)
	if err != nil {
		return err
	}
	switch action {
	case ControlPause:
		if !r.Active() {
			return errors.New("this review's loop has ended")
		}
		r.Paused = true
		r.note(now, "paused", "You", "Paused: no re-review or automatic send until you resume.", "")
	case ControlResume:
		if !r.Active() {
			return errors.New("this review's loop has ended")
		}
		r.Paused = false
		r.note(now, "resumed", "You", "Resumed.", "")
	case ControlStop:
		if !r.Active() {
			return nil
		}
		r.stop(now, StopStopped, "You stopped the loop.")
	case ControlTakeOver:
		if !r.Active() {
			return nil
		}
		r.stop(now, StopTakenOver, "You took over: the loop stopped; the conversations are yours.")
	case ControlLoop:
		m, err := LoopMode(mode)
		if err != nil {
			return err
		}
		r.Loop = m
		if maxRounds > 0 {
			r.MaxRounds = max(ClampRounds(maxRounds), r.Round)
		}
		words := map[string]string{LoopAsk: "you send findings between rounds", LoopAuto: "findings go to the author automatically", LoopOff: "no re-review"}
		r.note(now, "mode", "You", fmt.Sprintf("Loop: %s, up to %s.", words[m], plural(r.MaxRounds, "round", "rounds")), "")
	default:
		return fmt.Errorf("unknown review control %q", action)
	}
	return nil
}

// ErrReviewEnded refuses to cancel a review that has already ended.
var ErrReviewEnded = errors.New("this review has already ended")

// InProgress reports whether a review is still under way: its reviewer
// works, or its loop waits for the next round.
func (r Run) InProgress() bool { return r.State == RunRunning || r.Active() }

// endByHand ends a review the person ended: the loop stops, the round under
// way closes where it is, and questions still waiting go unanswered.
func (r *Run) endByHand(now int64, state, reason, by, text string) {
	if round := r.current(); round != nil && round.EndedAt == 0 {
		round.EndedAt = now
	}
	for i := range r.Asks {
		if r.Asks[i].State == AskPending {
			r.Asks[i].State = AskExpired
		}
	}
	r.State, r.StopReason, r.Phase, r.Paused = state, reason, PhaseDone, false
	r.StoppedBy, r.StoppedAt, r.UpdatedAt = by, now, now
	r.Detail = text
	r.note(now, reason, by, text, "")
}

// Cancel is the person's cancel of a review under way (CancelReview): no
// further round starts, and the findings filed so far are kept. The Runner
// then interrupts the reviewer's turn and ends its session.
func Cancel(rv *Review, runID, by string, now int64) (Run, error) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Run{}, err
	}
	if r.State == RunCancelled {
		return *r, errUnchanged
	}
	if !r.InProgress() {
		return *r, ErrReviewEnded
	}
	kept := CountFindings(*rv, r.ID).Total()
	who := firstNonEmpty(by, "You")
	text := fmt.Sprintf("%s cancelled the review; %s kept.", who, plural(kept, "finding", "findings"))
	if r.Round > 0 {
		text = fmt.Sprintf("%s cancelled the review in round %d; %s kept.", who, r.Round, plural(kept, "finding", "findings"))
	}
	r.endByHand(now, RunCancelled, StopCancelled, who, text)
	return *r, nil
}

// How the person ended a reviewer by hand.
const (
	EndedInterrupted = "interrupted"
	EndedSession     = "ended"
	EndedDeleted     = "deleted"
)

// ReviewerEnded follows the person ending a review's reviewer by hand: its
// turn stopped while it reviewed, its session ended, or its conversation
// deleted. The review stops there rather than waiting on a reviewer that
// will not report; a stop that interrupted no review changes nothing.
func ReviewerEnded(rv *Review, runID, how, by string, now int64) (Run, error) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Run{}, err
	}
	reviewing := r.State == RunRunning && (r.Phase == "" || r.Phase == PhaseReviewing)
	if !r.InProgress() || (how == EndedInterrupted && !reviewing) {
		return *r, errUnchanged
	}
	who := firstNonEmpty(by, "You")
	text := map[string]string{
		EndedInterrupted: fmt.Sprintf("%s stopped %s's turn; the review stopped.", who, r.ReviewerLabel),
		EndedSession:     fmt.Sprintf("%s ended %s's conversation; the review stopped.", who, r.ReviewerLabel),
		EndedDeleted:     fmt.Sprintf("%s deleted %s's conversation; the review stopped.", who, r.ReviewerLabel),
	}[how]
	if text == "" {
		return Run{}, fmt.Errorf("unknown way to end a reviewer %q", how)
	}
	r.endByHand(now, RunStopped, StopEnded, who, text)
	return *r, nil
}

// Verdict is the reviewer's resolve or reopen of one of its findings.
func Verdict(rv *Review, runID, findingID, state, note string, now int64) (Comment, error) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Comment{}, err
	}
	c, err := find(rv, findingID)
	if err != nil {
		return Comment{}, err
	}
	if c.ReviewID != r.ID {
		return Comment{}, fmt.Errorf("finding %s is not one of this review's: %w", findingID, ErrNotFound)
	}
	note = strings.Join(strings.Fields(note), " ")
	if len(note) > 2000 {
		return Comment{}, errors.New("a note is limited to 2000 bytes")
	}
	switch state {
	case "resolved":
		c.Disposition, c.ResolvedBy = Resolved, ""
		c.Note = "Resolved by " + r.ReviewerLabel + suffix(note)
		r.note(now, "resolved", r.ReviewerLabel, fmt.Sprintf("Resolved “%s”%s", c.Title, suffix(note)), c.ID)
	case "open":
		// Back to pending: it goes to the author again with the next findings.
		c.Disposition, c.ResolvedBy = Open, ""
		c.SentTo, c.SentToTitle, c.SentAt, c.SentHead = "", "", 0, ""
		c.Note = "Still open (" + r.ReviewerLabel + ")" + suffix(note)
		r.note(now, "reopened", r.ReviewerLabel, fmt.Sprintf("Still open: “%s”%s", c.Title, suffix(note)), c.ID)
	default:
		return Comment{}, errors.New(`a finding is "resolved" or "open"`)
	}
	c.UpdatedAt = now
	return *c, nil
}

func suffix(note string) string {
	if note == "" {
		return ""
	}
	return ": " + note
}
