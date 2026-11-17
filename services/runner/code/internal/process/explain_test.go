package process

import (
	"strings"
	"testing"
)

// Explain lists rules in file order, then role triggers in role order, then
// notifications, each with the scopes of the role it involves.
func TestReactionsAreInFiringOrder(t *testing.T) {
	e := Resolve(nil, nil, Options{Preset: ReviewedTeam})
	for _, tc := range []struct {
		event string
		want  []string // path: summary
	}{
		{"verdict.recorded", []string{
			"rules[2]: create a fix task for worker",
			"rules[3]: start reviewer",
			"roles.coordinator.triggers[1]: wake coordinator",
		}},
		{"task.reported", []string{
			"rules[0]: start verifier",
			`rules[1]: send "Report rejected: run the tests with lazybox run." to worker`,
		}},
		{"gate.requested", []string{"notifications[0]: notify inbox and desktop"}},
		{"timer", []string{"roles.transition.triggers[0]: wake transition"}},
		{"task.answered", nil},
	} {
		t.Run(tc.event, func(t *testing.T) {
			reactions, err := e.Process.Reactions(tc.event)
			if err != nil {
				t.Fatal(err)
			}
			var got []string
			for _, r := range reactions {
				got = append(got, r.Path+": "+r.Summary)
			}
			if strings.Join(got, "\n") != strings.Join(tc.want, "\n") {
				t.Fatalf("got\n%s\nwant\n%s", strings.Join(got, "\n"), strings.Join(tc.want, "\n"))
			}
		})
	}

	reactions, _ := e.Process.Reactions("task.reported")
	verifier := reactions[0]
	if verifier.Role != "verifier" || verifier.Permission != "read_only" || strings.Join(verifier.Scopes, ",") != "verdict.record,evidence.read" {
		t.Fatalf("verifier reaction: %+v", verifier)
	}
	if verifier.Condition != "task.evidence.exists(e, e.kind == 'run' && e.exit == 0)" {
		t.Fatalf("condition: %q", verifier.Condition)
	}
	if strings.Join(verifier.Emits, ",") != "attempt.started,attempt.ended,verdict.recorded" {
		t.Fatalf("emits: %v", verifier.Emits)
	}
	if _, err := e.Process.Reactions("task.exploded"); err == nil {
		t.Fatal("unknown event explained")
	}
}

func TestFormatReactions(t *testing.T) {
	e := Resolve(nil, nil, Options{Preset: SoloPair})
	reactions, err := e.Process.Reactions("finding.filed")
	if err != nil {
		t.Fatal(err)
	}
	want := `On finding.filed, in order:
 1. send snippet fixall to worker  [rules[1]]
    if   finding.severity == 'blocker'
    role worker (ask), scopes: task.report, task.question, evidence.read
    may cause prompt.submitted, snippet.sent, attempt.ended, task.reported, task.question
`
	if got := FormatReactions("finding.filed", reactions); got != want {
		t.Fatalf("got\n%s\nwant\n%s", got, want)
	}
	if got := FormatReactions("task.answered", nil); got != "Nothing reacts to task.answered.\n" {
		t.Fatalf("empty: %q", got)
	}
}
