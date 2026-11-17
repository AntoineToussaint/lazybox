package telemetry

import (
	"context"
	"testing"
	"time"
)

// The series counts each record once, in its hour, as the groups do: a
// retried response is not counted twice, an unpriced record adds tokens but
// no cost, and a report that did not ask for it has none.
func TestSeriesCountsRecordsByHourAsTheGroupsDo(t *testing.T) {
	s := store(t)
	nine := time.Date(2026, 11, 16, 9, 12, 0, 0, time.UTC)
	put := func(r Record) {
		t.Helper()
		if err := s.Put(context.Background(), r); err != nil {
			t.Fatal(err)
		}
	}
	base := Record{Scope: scope(), Status: "complete", Source: "gateway", Model: "known"}
	first := base
	first.ID, first.ResponseID, first.Nanos, first.ObservedAt = "a", "one", number(100), nine
	first.Tokens = Tokens{Input: number(2), CacheRead: number(8), Output: number(1)}
	put(first)
	retry := first
	retry.ID = "a-retry"
	put(retry)
	later := base
	later.ID, later.ResponseID, later.Nanos, later.ObservedAt = "b", "two", number(50), nine.Add(40*time.Minute)
	later.Tokens = Tokens{Input: number(1)}
	put(later)
	unpriced := base
	unpriced.ID, unpriced.ResponseID, unpriced.Model, unpriced.Reason, unpriced.ObservedAt = "c", "three", "mystery", "No price", nine.Add(26*time.Hour)
	unpriced.Tokens = Tokens{Output: number(7)}
	put(unpriced)

	if plain := read(t, s); plain.Series != nil {
		t.Fatalf("series without asking: %+v", plain.Series)
	}
	report, err := s.Report(context.Background(), Query{Workspace: "workspace", Series: true})
	if err != nil {
		t.Fatal(err)
	}
	if len(report.Series) != 2 {
		t.Fatalf("buckets = %+v", report.Series)
	}
	hour, next := report.Series[0], report.Series[1]
	if hour.HourUnixMillis != nine.Truncate(time.Hour).UnixMilli() || hour.Conversation != "conversation" || hour.Provider != "claude" || hour.Model != "known" {
		t.Fatalf("first bucket = %+v", hour)
	}
	if value(hour.Nanos) != 150 || hour.Records != 2 || value(hour.Tokens.Input) != 3 || value(hour.Tokens.CacheRead) != 8 || hour.Unpriced != 0 {
		t.Fatalf("first bucket counts = %+v", hour)
	}
	if next.HourUnixMillis != nine.Add(26*time.Hour).Truncate(time.Hour).UnixMilli() || next.Nanos != nil || next.Unpriced != 1 || value(next.Tokens.Output) != 7 {
		t.Fatalf("second bucket = %+v", next)
	}
	var groups, series int64
	for _, g := range report.Groups {
		groups += value(g.Nanos)
	}
	for _, b := range report.Series {
		series += value(b.Nanos)
	}
	if groups != series {
		t.Fatalf("groups cost %d, series %d", groups, series)
	}
}
