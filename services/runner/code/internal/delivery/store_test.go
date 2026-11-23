package delivery

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

func catalogContract(t *testing.T, version uint32, digest string) Contract {
	t.Helper()
	c, err := ContractFromSpec("spec-catalog", version, digest, example)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func TestCheckObserve(t *testing.T) {
	code := func(n int) *int { return &n }
	check := Check{CriterionID: "api", ContractDigest: "d", Attempt: "a", Subject: Subject{Conversation: "c"}, Artifact: "spec-catalog", Version: 3}
	needs := check
	needs.Prerequisite = true
	for _, tc := range []struct {
		name      string
		check     Check
		exit      *int
		signalled bool
		want      string
	}{
		{"check exit 0", check, code(0), false, Passed},
		{"check exit 1", check, code(1), false, Failed},
		{"check not found", check, code(127), false, Blocked},
		{"check not executable", check, code(126), false, Blocked},
		{"check stopped", check, code(143), true, Unverified},
		{"check piped", check, nil, false, Unverified},
		{"needs exit 1", needs, code(1), false, Blocked},
		{"needs exit 0", needs, code(0), false, Unverified},
		{"needs stopped", needs, code(1), true, Unverified},
	} {
		t.Run(tc.name, func(t *testing.T) {
			o := tc.check.Observe("run:e/c", "run:e/c", "candidate:1", tc.exit, tc.signalled)
			if err := o.Validate(); err != nil || o.Classify() != tc.want {
				t.Fatalf("%+v: %v %s", o, err, o.Classify())
			}
		})
	}
	met := needs.Observe("run:e/c", "run:e/c", "candidate:1", code(0), false)
	if got := met.Summary(needs); !strings.Contains(got, "prerequisite api: met") || !strings.Contains(got, "spec-catalog v3") {
		t.Fatal(got)
	}
}

func TestStoreBindsAndReplaysObservationsOnce(t *testing.T) {
	path := filepath.Join(t.TempDir(), "delivery.jsonl")
	s, err := OpenStore(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := s.Bind("conv", Contract{Schema: "spec.v1", Legacy: []string{"text"}}); err == nil {
		t.Fatal("bound an unnamed spec.v1 contract")
	}
	first, err := s.Bind("conv", catalogContract(t, 3, "d3"))
	if err != nil || first.Attempt != "conversation:conv/1" {
		t.Fatal(first, err)
	}
	titled := catalogContract(t, 3, "d3")
	titled.Title = "Catalog"
	again, _ := s.Bind("conv", titled)
	if again.Contract.Title != "Catalog" {
		t.Fatal("re-binding the same revision did not fill in its title", again)
	}
	next, _ := s.Bind("conv", catalogContract(t, 4, "d4"))
	other, _ := s.Bind("other", catalogContract(t, 3, "d3"))
	if again.Attempt != first.Attempt || next.Attempt != "conversation:conv/2" || other.Attempt != "conversation:other/1" {
		t.Fatal(again, next, other)
	}
	zero := 0
	check := Check{CriterionID: "api", ContractDigest: "d4", Attempt: next.Attempt, Subject: Subject{Conversation: "conv"}}
	o := check.Observe("run:e/1", "run:e/1", "candidate:1", &zero, false)
	if added, err := s.Record(o); !added || err != nil {
		t.Fatal(added, err)
	}
	if added, err := s.Record(o); added || err != nil {
		t.Fatal("a replay added", added, err)
	}
	changed := o
	changed.Assertion = Failed
	if _, err := s.Record(changed); !errors.Is(err, ErrConflict) {
		t.Fatal("rewrote a recorded observation", err)
	}
	invalid := o
	invalid.ID = "run:e/2"
	invalid.Candidate = ""
	if _, err := s.Record(invalid); err == nil {
		t.Fatal("recorded an observation with no candidate")
	}
	_ = s.Close()

	// A torn last line was never acknowledged: reopening drops it and keeps the rest.
	// A damaged line is skipped rather than keeping the Runner from starting.
	f, _ := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
	_, _ = f.WriteString("{damaged\n" + `{"observation":{"schema":"delivery-obs`)
	_ = f.Close()
	s, err = OpenStore(path)
	if err != nil || s.Skipped() != 1 {
		t.Fatal(err)
	}
	defer s.Close()
	if b, ok := s.Binding("conv"); !ok || b.Attempt != next.Attempt {
		t.Fatal(b, ok)
	}
	if got := s.Observations(); len(got) != 1 || got[0].ID != "run:e/1" {
		t.Fatal(got)
	}
	if added, err := s.Record(o); added || err != nil {
		t.Fatal("a replay after reopening added", added, err)
	}
	o2 := check.Observe("run:e/3", "run:e/3", "candidate:1", &zero, false)
	if added, err := s.Record(o2); !added || err != nil {
		t.Fatal(added, err)
	}
	if a := Assess(o.Scope, next.Contract, s.Observations()); a.Criteria["api"] != Passed {
		t.Fatal(a)
	}
}

func TestStoreKeepsConcurrentAttemptsDistinct(t *testing.T) {
	s, err := OpenStore(filepath.Join(t.TempDir(), "delivery.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	contract := catalogContract(t, 3, "d3")
	zero, one := 0, 1
	var wg sync.WaitGroup
	for i, attempt := range []string{"attempt-1", "attempt-2"} {
		exit := &zero
		if i == 1 {
			exit = &one
		}
		check := Check{CriterionID: "storage", ContractDigest: "d3", Attempt: attempt, Subject: Subject{Conversation: attempt}}
		for n := range 20 {
			wg.Add(1)
			go func() {
				defer wg.Done()
				id := "run:" + attempt + "/" + string(rune('a'+n%5))
				if _, err := s.Record(check.Observe(id, id, "candidate:1", exit, false)); err != nil {
					t.Error(err)
				}
			}()
		}
	}
	wg.Wait()
	if got := len(s.Observations()); got != 10 {
		t.Fatalf("%d observations, want 5 per attempt", got)
	}
	for attempt, want := range map[string]string{"attempt-1": Passed, "attempt-2": Failed} {
		if a := Assess(Scope{"d3", "candidate:1", attempt}, contract, s.Observations()); a.Criteria["storage"] != want {
			t.Fatal(attempt, a)
		}
	}
}
