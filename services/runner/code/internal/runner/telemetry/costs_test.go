package telemetry

import (
	"context"
	"reflect"
	"testing"
)

// A cost says what it is made of: each part of a priced record (input, cache
// read, cache writes, output) is priced on its own, a group adds the parts
// up, and what has no price says why and how many tokens it leaves out.
func TestReportBreaksCostDownByPartAndSaysWhatIsUnpriced(t *testing.T) {
	ctx := context.Background()
	s := store(t)
	rates := DefaultRates()
	base := Record{Scope: scope(), Agent: "root", Source: "sdk", Model: "claude-sonnet-4-6", Status: "complete", Tokens: Tokens{Input: number(100), Output: number(20), CacheRead: number(1000), CacheWrite5m: number(200), CacheWrite1h: number(0)}}
	fresh := rates.Price(base)
	fresh.ID, fresh.ResponseID = "fresh", "one"
	want := &Costs{Input: number(300000), Output: number(300000), CacheRead: number(300000), CacheWrite5m: number(750000), CacheWrite1h: number(0)}
	if !reflect.DeepEqual(fresh.Costs, want) || *fresh.Nanos != 1650000 {
		t.Fatalf("parts of a price: %+v", fresh.Costs)
	}
	// Priced before parts were stored, at the rate still current: its parts are exact.
	legacy := fresh
	legacy.ID, legacy.ResponseID, legacy.Costs = "legacy", "two", nil
	tools := base
	tools.ID, tools.ResponseID, tools.Reason = "tools", "three", "Additional tool fees are not priced"
	// Another agent's usage, priced at a rate that has changed since: its total stands, its parts are not guessed.
	old := base
	old.ID, old.ResponseID, old.Agent, old.Nanos, old.PriceVersion = "old", "four", "helper", number(999), "anthropic-standard-2025"
	for _, r := range []Record{fresh, legacy, tools, old} {
		if err := s.Put(ctx, r); err != nil {
			t.Fatal(err)
		}
	}
	report, err := s.Report(ctx, Query{Workspace: "workspace", Prices: rates})
	if err != nil {
		t.Fatal(err)
	}
	groups := map[string]Group{}
	for _, g := range report.Groups {
		groups[g.Agent] = g
	}
	root := groups["root"]
	if root.Nanos == nil || *root.Nanos != 3300000 || root.Unpriced != 1 {
		t.Fatalf("root total: %+v", root)
	}
	if !reflect.DeepEqual(root.Costs, &Costs{Input: number(600000), Output: number(600000), CacheRead: number(600000), CacheWrite5m: number(1500000), CacheWrite1h: number(0)}) {
		t.Fatalf("root parts: %+v", root.Costs)
	}
	if !reflect.DeepEqual(root.UnpricedReasons, []string{"Additional tool fees are not priced"}) || !reflect.DeepEqual(root.UnpricedTokens, base.Tokens) {
		t.Fatalf("what is unpriced and why: %v %+v", root.UnpricedReasons, root.UnpricedTokens)
	}
	helper := groups["helper"]
	if helper.Nanos == nil || *helper.Nanos != 999 || helper.Costs != nil || helper.Unpriced != 0 {
		t.Fatalf("a price whose parts are unknown: %+v", helper)
	}
}
