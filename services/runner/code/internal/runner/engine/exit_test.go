package engine

import (
	"errors"
	"fmt"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A native process whose exit was observed is not a lost stream: the
// conversation's fault is agent.ProcessExitFault, whether the exit came as the
// stream's error or as the supervising layer's report of it, and neither a
// re-adoption nor a later event settles it.
func TestAnObservedExitIsNotSettledByReadoption(t *testing.T) {
	for _, name := range []string{"stream error", "exit report"} {
		t.Run(name, func(t *testing.T) {
			e, native, st, path := durableEngine(t)
			native.Events <- agent.Event{Type: "session_ready", Cursor: 1}
			await(t, e, "session_ready")
			if name == "stream error" {
				native.End(fmt.Errorf("%w (code 1)", agent.ErrProcessExited))
			} else {
				native.Events <- agent.Event{Type: "process_exit", Status: "exited", Text: "native process exited (code 1)"}
				native.End(errors.New("provider supervisor ended"))
			}
			for end := time.Now().Add(5 * time.Second); e.Streaming(); time.Sleep(5 * time.Millisecond) {
				if time.Now().After(end) {
					t.Fatal("the stream never ended")
				}
			}
			if s := e.Snapshot(); s.Fault != agent.ProcessExitFault {
				t.Fatalf("an observed exit faulted as %q", s.Fault)
			}
			if err := e.Detach(); err != nil {
				t.Fatal(err)
			}
			next, a, err := adopt(t, path, st)
			if err != nil {
				t.Fatal(err)
			}
			if s := next.Snapshot(); s.Fault != agent.ProcessExitFault {
				t.Fatalf("re-adoption cleared an exit: %+v", s)
			}
			a.Session.Events <- agent.Event{Type: "activity", Status: "requesting", Cursor: 2}
			await(t, next, "activity")
			if s := next.Snapshot(); s.Fault != agent.ProcessExitFault {
				t.Fatalf("an event cleared an exit: %+v", s)
			}
		})
	}
}
