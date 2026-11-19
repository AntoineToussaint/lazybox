package telemetry

import "testing"

func TestCurrentModelRates(t *testing.T) {
	c := DefaultRates()
	for _, tt := range []struct {
		provider, model string
		total           int64
	}{
		{"codex", "gpt-6.2-astra", 48400},
		{"codex", "gpt-6.2-sol", 12100},
		{"codex", "gpt-6.1-sol", 12100},
		{"codex", "gpt-6.1-terra", 6050},
		{"codex", "gpt-6-astra", 61000},
		{"codex", "gpt-6-sol", 12200},
		{"codex", "gpt-6-luna", 610},
		{"codex", "gpt-5.5", 35500},
		{"claude", "claude-fable-5-2", 48200},
		{"claude", "claude-haiku-5-5", 610},
	} {
		r := c.Price(Record{
			Scope: Scope{Provider: tt.provider}, Model: tt.model, Status: "complete",
			Tokens: Tokens{Input: number(1), Output: number(1), CacheRead: number(1), CacheWrite5m: number(0), CacheWrite1h: number(0)},
		})
		if r.Nanos == nil || *r.Nanos != tt.total {
			t.Errorf("%s: expected %d nanos, got %+v", tt.model, tt.total, r)
		}
	}
}

func TestHaikuLongContextIncludesCacheWrites(t *testing.T) {
	r := Record{
		Scope: Scope{Provider: "claude"}, Model: "claude-haiku-5-5", Status: "complete",
		Tokens: Tokens{Input: number(1), Output: number(1), CacheRead: number(1), CacheWrite5m: number(99997), CacheWrite1h: number(1)},
	}
	c := DefaultRates()
	if got := c.Price(r); got.Nanos == nil {
		t.Fatalf("100K input should be priced: %s", got.Reason)
	}
	r.Tokens.CacheWrite1h = number(2)
	if got := c.Price(r); got.Nanos != nil || got.Reason != "Long-context price requires an explicit rate" {
		t.Fatalf("100001 input should require a long-context rate: %+v", got)
	}
}
