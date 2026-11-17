package process

import (
	"context"
	"strings"
	"testing"
	"time"
)

func TestConditionsEvaluate(t *testing.T) {
	c, err := CompileCondition("task.evidence.exists(e, e.kind == 'run' && e.exit == 0)", "task.reported")
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(c.Vars(), ",") != "event,mission,task" {
		t.Fatalf("vars: %v", c.Vars())
	}
	ctx := context.Background()
	for _, tc := range []struct {
		evidence []any
		want     bool
	}{
		{[]any{map[string]any{"kind": "run", "exit": 0}}, true},
		{[]any{map[string]any{"kind": "run", "exit": 1}}, false},
		{[]any{map[string]any{"kind": "commit", "exit": 0}}, false},
		{nil, false},
	} {
		got, err := c.Eval(ctx, map[string]any{"task": map[string]any{"evidence": tc.evidence}})
		if err != nil || got != tc.want {
			t.Fatalf("evidence %v: %v %v, want %v", tc.evidence, got, err, tc.want)
		}
	}
	// A variable the caller leaves out reads as empty: a field of it is an
	// error, never a silent false.
	if _, err = c.Eval(ctx, nil); err == nil || !strings.Contains(err.Error(), "no such key") {
		t.Fatalf("missing task: %v", err)
	}
	gate, err := CompileGateCondition("pr.checks_green && pr.merged")
	if err != nil {
		t.Fatal(err)
	}
	if ok, err := gate.Eval(ctx, map[string]any{"pr": map[string]any{"checks_green": true, "merged": true}}); !ok || err != nil {
		t.Fatalf("gate: %v %v", ok, err)
	}
}

// Conditions come from untrusted repository files: their size, depth,
// nesting and evaluation cost are all bounded.
func TestConditionsAreBounded(t *testing.T) {
	for _, tc := range []struct {
		name, expr, text string
	}{
		{"too long", "task.a == '" + strings.Repeat("x", MaxConditionBytes) + "'", "the limit is 1024"},
		{"too deep", strings.Repeat("(", 40) + "true" + strings.Repeat(")", 40), "recursion"},
		{"nested comprehensions", "mission.a.all(x, mission.b.all(y, mission.c.all(z, x == y && y == z)))", "nesting"},
		{"not a bool", "'yes'", "must be true or false"},
		{"empty", "  ", "empty condition"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			_, err := CompileCondition(tc.expr, "task.reported")
			if err == nil || !strings.Contains(err.Error(), tc.text) {
				t.Fatalf("got %v, want %q", err, tc.text)
			}
		})
	}

	// Two nested comprehensions over large lists are allowed to compile but
	// stop at the cost limit instead of running four million iterations.
	// (The deadline is lifted so the cost limit, not a loaded machine, stops it.)
	defer func(d time.Duration) { conditionDeadline = d }(conditionDeadline)
	conditionDeadline = time.Minute
	c, err := CompileCondition("mission.tasks.all(a, mission.tasks.all(b, a <= b || b <= a))", "mission.created")
	if err != nil {
		t.Fatal(err)
	}
	tasks := make([]any, 2000)
	for i := range tasks {
		tasks[i] = i
	}
	_, err = c.Eval(context.Background(), map[string]any{"mission": map[string]any{"tasks": tasks}})
	if err == nil || !strings.Contains(err.Error(), "cost limit") {
		t.Fatalf("got %v, want the cost limit", err)
	}
	// A small list evaluates normally under the same program.
	ok, err := c.Eval(context.Background(), map[string]any{"mission": map[string]any{"tasks": tasks[:10]}})
	if err != nil || !ok {
		t.Fatalf("small list: %v %v", ok, err)
	}
	// A canceled caller, or the deadline, stops evaluation.
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err = c.Eval(ctx, map[string]any{"mission": map[string]any{"tasks": tasks[:200]}}); err == nil {
		t.Fatal("evaluation ignored a canceled context")
	}
	conditionDeadline = time.Nanosecond
	if _, err = c.Eval(context.Background(), map[string]any{"mission": map[string]any{"tasks": tasks[:200]}}); err == nil || !strings.Contains(err.Error(), "deadline") {
		t.Fatalf("got %v, want the deadline", err)
	}
}
