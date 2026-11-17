package items

import (
	"time"
)

// How often a linked item is read again. Open pull requests change as checks
// run and reviews arrive: about every minute while a window has focus, every
// five minutes otherwise. Ended ones change rarely, so once a day, until a
// week after they ended. Conditional requests make an unchanged read free of
// rate limit, but each still costs a round trip.
const (
	OpenFocused   = 60 * time.Second
	OpenUnfocused = 5 * time.Minute
	EndedInterval = 24 * time.Hour
	EndedFor      = 7 * 24 * time.Hour
	// BranchFor: a branch no longer checked out is still asked about for a
	// week, for a pull request opened from it later.
	BranchFor = 7 * 24 * time.Hour
	// backoffMax caps the wait after reads that keep failing.
	backoffMax = 30 * time.Minute
)

// Interval is how often open items are read.
func Interval(focused bool) time.Duration {
	if focused {
		return OpenFocused
	}
	return OpenUnfocused
}

// Due reports whether a tracked item should be read now, and when it next
// will be otherwise. An item never read is due at once, and again after a
// failed read only as reads that keep failing back off: one that does not
// exist, or that the connection cannot see (404), is not asked about on
// every pass.
func Due(t TrackedItem, now time.Time, focused bool) (bool, time.Time) {
	if t.Tracking.CheckedAt.IsZero() || (!t.Known && t.Tracking.Failures == 0) {
		return true, now
	}
	every := Interval(focused)
	if Ended(t.Item.State) {
		ended := t.Item.ClosedAt
		if ended.IsZero() {
			ended = t.Item.ObservedAt
		}
		if now.Sub(ended) > EndedFor {
			return false, time.Time{}
		}
		every = EndedInterval
	}
	every = backoff(every, t.Tracking.Failures)
	next := t.Tracking.CheckedAt.Add(every)
	return !now.Before(next), next
}

// BranchDue reports whether a branch should be asked about now: seen within
// the last week, and not asked about for an interval. A branch not known to
// be pushed is asked about at the unfocused interval: whether it was pushed
// is often not visible locally (Branch.Pushed), and an unchanged answer is a
// conditional 304. Lookups that keep failing back off.
func BranchDue(b Branch, now time.Time, focused bool) bool {
	if now.Sub(b.LastSeen) > BranchFor {
		return false
	}
	every := Interval(focused)
	if !b.Pushed {
		every = OpenUnfocused
	}
	return b.CheckedAt.IsZero() || !now.Before(b.CheckedAt.Add(backoff(every, b.Failures)))
}

// backoff doubles the interval per failure in a row, up to backoffMax (an
// interval already longer than that stays as it is).
func backoff(every time.Duration, failures int) time.Duration {
	if every >= backoffMax {
		return every
	}
	for i := 0; i < failures && every < backoffMax; i++ {
		every *= 2
	}
	return min(every, backoffMax)
}
