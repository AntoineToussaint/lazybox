package runner

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"math"
	"regexp"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// turnStart reports whether a journal value is a message the person sent: a
// turn of the conversation starts there, and a reader can show the
// conversation from it without anything recorded before. Messages Lazybox
// wrote to the agent (an origin, or one of the operation ids the Runner gives
// the messages it writes), answers to the agent's requests, an interrupt (no
// text) and a sub-agent's operations are not.
func turnStart(value json.RawMessage) bool {
	if !bytes.Contains(value, []byte(`"operation_accepted"`)) {
		return false
	}
	var e struct {
		Type        string `json:"type"`
		Text        string `json:"text"`
		RequestID   string `json:"requestId"`
		OperationID string `json:"operationId"`
		AgentID     string `json:"agentId"`
		Data        struct {
			Origin json.RawMessage `json:"origin"`
		} `json:"data"`
	}
	if json.Unmarshal(value, &e) != nil {
		return false
	}
	origin := len(e.Data.Origin) > 0 && !bytes.Equal(e.Data.Origin, []byte("null"))
	return e.Type == "operation_accepted" && e.Text != "" && e.RequestID == "" && (e.AgentID == "" || e.AgentID == "root") && !origin && !lazyboxOperation(e.OperationID)
}

// lazyboxOperation: the operation ids of messages the Runner writes, which
// journals from before origins were recorded identify them by.
var lazyboxOperationIDs = regexp.MustCompile(`^review-.+-(findings|round)-\d+$`)

func lazyboxOperation(id string) bool {
	for _, prefix := range []string{"repository-note-", "review-brief-", "review-ask-", "review-answer-"} {
		if strings.HasPrefix(id, prefix) {
			return true
		}
	}
	return lazyboxOperationIDs.MatchString(id)
}

// Timeline reads a conversation's timeline: the records of every attempt of
// its lineage, oldest first, numbered contiguously. Only the last attempt can
// grow. The lineage is resolved once, so a long replay does not look the
// conversation up again for each record.
type Timeline struct {
	chain []*Execution // oldest first
	// caps bounds the records read from each attempt, parallel to chain. An
	// attempt that replaced the conversation from one of the person's
	// messages on (sessionstore.Record.Cut) hides what its predecessors
	// recorded from that cursor: the timeline is then the conversation as it
	// stands, and the replaced records stay in their journals.
	caps []uint64
}

// count is the number of records the timeline reads from attempt i.
func (t *Timeline) count(i int) uint64 {
	count := t.chain[i].spool.Count()
	if i < len(t.caps) {
		count = min(count, t.caps[i])
	}
	return count
}

// attempt is the attempt that recorded the record at cursor; nil when none did.
func (t *Timeline) attempt(cursor uint64) *Execution {
	var offset uint64
	for i, e := range t.chain {
		if offset += t.count(i); cursor <= offset {
			return e
		}
	}
	return nil
}

// OpenTimeline resolves id's lineage for reading its timeline.
func (b *Bench) OpenTimeline(id string) (*Timeline, error) {
	chain, err := b.lineage(id)
	if err != nil {
		return nil, err
	}
	t := &Timeline{chain: make([]*Execution, len(chain)), caps: make([]uint64, len(chain))}
	for i, e := range chain {
		t.chain[len(chain)-1-i] = e
	}
	for i, e := range t.chain {
		t.caps[i] = math.MaxUint64
		if cut := e.Saved.Cut; cut > 0 {
			// The cursor counts what was visible when the cut was made, so
			// earlier cuts are already applied.
			kept := cut - 1
			for earlier := 0; earlier < i; earlier++ {
				t.caps[earlier] = min(t.count(earlier), kept)
				kept -= t.caps[earlier]
			}
		}
	}
	return t, nil
}

// Retained is the last cursor of the timeline.
func (t *Timeline) Retained() uint64 {
	var total uint64
	for i := range t.chain {
		total += t.count(i)
	}
	return total
}

// Next returns the record after cursor, waiting for the last attempt to
// record it; io.EOF once that attempt's journal is closed.
func (t *Timeline) Next(ctx context.Context, cursor uint64) (journal.Record, error) {
	var offset uint64
	for i, e := range t.chain {
		count := t.count(i)
		if cursor < offset+count || i == len(t.chain)-1 {
			if cursor < offset {
				return journal.Record{}, journal.ErrCursor
			}
			r, err := e.Engine.Events(ctx, cursor-offset)
			r.Sequence += offset
			return r, err
		}
		offset += count
	}
	return journal.Record{}, io.EOF
}

// Page reads the records of the timeline that end just before cursor
// `before` (0: its last record), latest first in what it keeps: at most
// about maxBytes of journal, and, when turns is positive, starting where one
// of the person's messages starts a turn, at most `turns` of them back. A
// last turn larger than maxBytes is cut to its latest records. At least one
// record is returned when there is any before `before`. start is the cursor
// of the first record (1: nothing is earlier); retained is the timeline's
// last cursor when it was read.
func (t *Timeline) Page(before uint64, turns int, maxBytes int64) (records []journal.Record, start, retained uint64, err error) {
	counts := make([]uint64, len(t.chain))
	for i := range t.chain {
		counts[i] = t.count(i)
		retained += counts[i]
	}
	end := retained
	if before > 0 {
		if before-1 > retained {
			return nil, 0, retained, journal.ErrCursor
		}
		end = before - 1
	}
	if end == 0 {
		return nil, 1, retained, nil
	}
	start = t.fit(counts, end, maxBytes)
	if turns > 0 {
		marks := t.marks(counts, end)
		aligned := uint64(1)
		if len(marks) >= turns {
			aligned = marks[len(marks)-turns]
		}
		if aligned >= start {
			start = aligned
		} else {
			// Not all of them fit: the most whole turns that do, else the
			// latest records of the last one.
			fitting := uint64(0)
			for i := len(marks) - 1; i >= 0 && marks[i] >= start; i-- {
				fitting = marks[i]
			}
			if fitting > 0 {
				start = fitting
			}
		}
	}
	var offset uint64
	for i, e := range t.chain {
		from, to := max(start, offset+1), min(end, offset+counts[i])
		if from <= to {
			part, err := e.spool.Range(from-1-offset, to-offset)
			if err != nil {
				return nil, 0, retained, err
			}
			for _, r := range part {
				r.Sequence += offset
				records = append(records, r)
			}
		}
		offset += counts[i]
	}
	return records, start, retained, nil
}

// fit is the earliest cursor from which the records up to end fit in budget
// bytes, and end itself when not even the last one does.
func (t *Timeline) fit(counts []uint64, end uint64, budget int64) uint64 {
	var offset uint64
	offsets := make([]uint64, len(t.chain))
	for i := range t.chain {
		offsets[i] = offset
		offset += counts[i]
	}
	start := end
	for i := len(t.chain) - 1; i >= 0; i-- {
		if end <= offsets[i] {
			continue
		}
		upTo := min(end, offsets[i]+counts[i]) - offsets[i]
		after := t.chain[i].spool.Fit(upTo, budget)
		budget -= t.chain[i].spool.Span(after, upTo)
		if after < upTo {
			start = offsets[i] + after + 1
		}
		if after > 0 {
			break
		}
	}
	return start
}

// marks are the timeline's turn starts up to end, ascending.
func (t *Timeline) marks(counts []uint64, end uint64) []uint64 {
	var marks []uint64
	var offset uint64
	for i, e := range t.chain {
		for _, m := range e.spool.Marks() {
			if m <= counts[i] && offset+m <= end {
				marks = append(marks, offset+m)
			}
		}
		offset += counts[i]
	}
	return marks
}
