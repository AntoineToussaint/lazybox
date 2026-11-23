package browser

import (
	"encoding/json"
	"errors"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

// The browser not opening reads as the service's words when it refused, and
// as one line of the process's failure otherwise; the typed error survives.
func TestTheBrowserSaysWhyItDidNotOpen(t *testing.T) {
	refused := error(jsonl.NewCallError("open", json.RawMessage(`{"message":"Chromium is not installed"}`)))
	got := openProblem(refused)
	var call *jsonl.CallError
	if got.Error() != "Chromium is not installed" || !errors.As(got, &call) {
		t.Fatalf("a refusal: %q", got)
	}
	if got := openProblem(errors.New("exit status 1\nstack line\nmore")); got.Error() != "the browser service did not start: exit status 1" {
		t.Fatalf("a process failure: %q", got)
	}
}
