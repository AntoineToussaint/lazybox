package codex

import "encoding/json"

// compactionState maps Codex's context compaction to the `compacted` event
// Claude's adapter records ("Compacted context from 156k tokens"). Codex says
// a compaction happened (a contextCompaction item, or the deprecated
// thread/compacted notification) but not its sizes: the size before is the
// main thread's context in use from its latest token usage (what telemetry
// reads as used), and the size after is not known. Lazybox never asks Codex
// to compact (its /compact says Codex compacts on its own), so every
// compaction is automatic.
type compactionState struct {
	used int64
	// said: turns whose compaction was already recorded, so the item and
	// the deprecated notification of one compaction make one line.
	said map[string]bool
}

// usage remembers the main thread's context in use (tokenUsage.last.totalTokens).
func (c *compactionState) usage(params json.RawMessage) {
	var u struct {
		TokenUsage struct {
			Last struct {
				TotalTokens int64 `json:"totalTokens"`
			} `json:"last"`
		} `json:"tokenUsage"`
	}
	if json.Unmarshal(params, &u) == nil && u.TokenUsage.Last.TotalTokens > 0 {
		c.used = u.TokenUsage.Last.TotalTokens
	}
}

// first reports whether a compaction of turn is new, and marks it said.
func (c *compactionState) first(turn string) bool {
	if turn == "" {
		return true
	}
	if c.said == nil || len(c.said) >= 256 {
		c.said = map[string]bool{}
	}
	if c.said[turn] {
		return false
	}
	c.said[turn] = true
	return true
}

// data is the compacted event's data: automatic, and the size before when known.
func (c *compactionState) data() json.RawMessage {
	data := map[string]any{"trigger": "auto"}
	if c.used > 0 {
		data["preTokens"] = c.used
	}
	out, _ := json.Marshal(data)
	return out
}

// compactionItem reports whether an item notification is a context compaction.
func compactionItem(params json.RawMessage) bool {
	var p struct {
		Item struct {
			Type string `json:"type"`
		} `json:"item"`
	}
	return json.Unmarshal(params, &p) == nil && p.Item.Type == "contextCompaction"
}
