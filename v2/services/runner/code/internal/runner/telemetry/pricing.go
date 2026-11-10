package telemetry

import "math"

// Rates are nanodollars/token (USD/million tokens * 1000), exact integers.
// Cards are exact model matches. Never guess a new model from a familiar prefix.
type Rate struct {
	Version      string `json:"version"`
	Input        int64  `json:"input"`
	Output       int64  `json:"output"`
	CacheRead    int64  `json:"cacheRead"`
	CacheWrite5m int64  `json:"cacheWrite5m"`
	CacheWrite1h int64  `json:"cacheWrite1h"`
	MaxInput     int64  `json:"maxInput,omitempty"`
}
type RateCard map[string]Rate

func DefaultRates() RateCard {
	c := RateCard{}
	c["claude/claude-fable-5-1"] = Rate{Version: "anthropic-standard-2026-11-03", Input: 10000, Output: 50000, CacheRead: 250, CacheWrite5m: 12500, CacheWrite1h: 20000}
	add := func(models []string, r Rate) {
		r.Version = "anthropic-standard-2026-11-02"
		for _, m := range models {
			c["claude/"+m] = r
		}
	}
	add([]string{"claude-opus-4-6", "claude-opus-4-7", "claude-opus-4-8", "claude-opus-5"}, Rate{Input: 5000, Output: 25000, CacheRead: 500, CacheWrite5m: 6250, CacheWrite1h: 10000})
	add([]string{"claude-opus-5-5"}, Rate{Input: 4000, Output: 20000, CacheRead: 200, CacheWrite5m: 5000, CacheWrite1h: 8000})
	add([]string{"claude-sonnet-4-6"}, Rate{Input: 3000, Output: 15000, CacheRead: 300, CacheWrite5m: 3750, CacheWrite1h: 6000})
	add([]string{"claude-sonnet-5"}, Rate{Input: 2000, Output: 10000, CacheRead: 200, CacheWrite5m: 2500, CacheWrite1h: 4000})
	add([]string{"claude-sonnet-5-5"}, Rate{Input: 2000, Output: 10000, CacheRead: 100, CacheWrite5m: 2500, CacheWrite1h: 4000})
	c["codex/gpt-5.4"] = Rate{Version: "openai-standard-2026-11-02", Input: 2500, Output: 15000, CacheRead: 250, MaxInput: 272000}
	c["codex/gpt-5.3-codex"] = Rate{Version: "openai-standard-2026-11-02", Input: 1750, Output: 14000, CacheRead: 175}
	return c
}
func (c RateCard) Price(r Record) Record {
	r.Nanos = nil
	r.PriceVersion = ""
	if r.Reason != "" {
		return r
	}
	rate, ok := c[r.Scope.Provider+"/"+r.Model]
	if !ok {
		r.Reason = "No exact model price"
		return r
	}
	if rate.Version == "" || rate.Input <= 0 || rate.Output <= 0 {
		r.Reason = "Incomplete rate card"
		return r
	}
	if r.Status != "complete" || r.Tokens.Input == nil || r.Tokens.Output == nil || r.Tokens.CacheRead == nil || r.Tokens.CacheWrite5m == nil || r.Tokens.CacheWrite1h == nil {
		r.Reason = "Incomplete usage"
		return r
	}
	if r.Tier != "" && r.Tier != "default" && r.Tier != "standard" {
		r.Reason = "Unpriced service tier"
		return r
	}
	if rate.MaxInput > 0 && *r.Tokens.Input+*r.Tokens.CacheRead > rate.MaxInput {
		r.Reason = "Long-context price requires an explicit rate"
		return r
	}
	var total int64
	for _, pair := range [][2]int64{{*r.Tokens.Input, rate.Input}, {*r.Tokens.Output, rate.Output}, {*r.Tokens.CacheRead, rate.CacheRead}, {*r.Tokens.CacheWrite5m, rate.CacheWrite5m}, {*r.Tokens.CacheWrite1h, rate.CacheWrite1h}} {
		if pair[1] < 0 || pair[0] < 0 || (pair[0] > 0 && pair[1] > (math.MaxInt64-total)/pair[0]) {
			r.Reason = "Invalid or overflowing price"
			return r
		}
		total += pair[0] * pair[1]
	}
	r.Nanos = number(total)
	r.PriceVersion = rate.Version
	return r
}
