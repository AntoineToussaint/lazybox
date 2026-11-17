package telemetry

import (
	"context"
	"testing"
	"time"
)

func TestConversationCostsIncludeEveryExecutionAndExcludeOtherConversations(t *testing.T) {
	s := store(t)
	ctx := context.Background()
	for index, execution := range []string{"first", "continued", "other"} {
		sc := scope()
		sc.Execution = execution
		if index == 2 {
			sc.Conversation = "elsewhere"
		}
		cost := int64(index+1) * 100
		if err := s.Put(ctx, Record{ID: execution, Scope: sc, Agent: "root", Model: "fixture", Source: "sdk", Status: "complete", Nanos: &cost, ObservedAt: time.Now()}); err != nil {
			t.Fatal(err)
		}
	}
	report, err := s.Report(ctx, Query{Conversation: "conversation"})
	if err != nil {
		t.Fatal(err)
	}
	var cost int64
	for _, group := range report.Groups {
		cost += value(group.Nanos)
	}
	if len(report.Groups) != 2 || cost != 300 {
		t.Fatal(report)
	}
	exact, err := s.Report(ctx, Query{Execution: "continued"})
	if err != nil || len(exact.Groups) != 1 || value(exact.Groups[0].Nanos) != 200 {
		t.Fatal(exact, err)
	}
}
