package commands

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"
)

// One execution, or one terminal, holds at most MaxOpenPipes open pipes: an
// agent that registers consoles and never finishes them cannot take every
// live slot of the registry. A finished one frees its place.
func TestOpenPipesAreBoundedPerExecutionAndTerminal(t *testing.T) {
	m := manager(t, 1<<20)
	var mine []*Command
	for i := 0; i < MaxOpenPipes; i++ {
		c, err := m.Register(Spec{SessionID: "agent", Title: "log"})
		if err != nil {
			t.Fatalf("pipe %d: %v", i, err)
		}
		mine = append(mine, c)
	}
	if _, err := m.Register(Spec{SessionID: "agent", Title: "one too many"}); err == nil || !strings.Contains(err.Error(), "consoles are already open") {
		t.Fatalf("past the bound: %v", err)
	}
	// Another execution and a terminal have their own.
	if _, err := m.Register(Spec{SessionID: "other", Title: "log"}); err != nil {
		t.Fatal(err)
	}
	for i := 0; i < MaxOpenPipes; i++ {
		if _, err := m.Register(Spec{CollectionID: "w", TerminalID: "t1", Title: "run"}); err != nil {
			t.Fatalf("terminal pipe %d: %v", i, err)
		}
	}
	if _, err := m.Register(Spec{CollectionID: "w", TerminalID: "t1", Title: "run"}); err == nil {
		t.Fatal("a terminal past the bound")
	}
	if err := mine[0].FinishPipe(nil); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Register(Spec{SessionID: "agent", Title: "after one finished"}); err != nil {
		t.Fatalf("a finished pipe frees its place: %v", err)
	}
}

// An unfollowed pipe that receives nothing is ended after a while, sooner
// when it never received anything; a followed one is the follower's.
func TestAnIdleUnfollowedPipeIsEnded(t *testing.T) {
	savedSilence, savedIdle := followSilence, pipeIdle
	followSilence = 300 * time.Millisecond
	pipeIdle.Never, pipeIdle.After = 200*time.Millisecond, time.Hour
	defer func() { followSilence, pipeIdle = savedSilence, savedIdle }()
	m := manager(t, 1<<20)
	abandoned, err := m.Register(Spec{SessionID: "agent", Title: "never written"})
	if err != nil {
		t.Fatal(err)
	}
	quiet, err := m.Register(Spec{SessionID: "agent", Title: "a quiet log"})
	if err != nil {
		t.Fatal(err)
	}
	if err = quiet.Publish(0, []byte("listening\n")); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	state, err := abandoned.Wait(ctx)
	if err != nil || !state.Done || !strings.Contains(state.Error, "nothing was published") {
		t.Fatalf("abandoned pipe: %+v %v", state, err)
	}
	if s := quiet.Status(); s.Done {
		t.Fatalf("a quiet log that published was ended within an hour: %+v", s)
	}
	short, stop := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer stop()
	if _, err = quiet.Wait(short); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("quiet log ended: %v", err)
	}
}
