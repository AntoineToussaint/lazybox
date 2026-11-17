package agent

import (
	"strings"
	"testing"
)

func TestPromptPreservesUserTextAndProvidesEscapedGenericFileReferences(t *testing.T) {
	if Prompt(Operation{Text: "plain"}) != "plain" {
		t.Fatal("plain prompt changed")
	}
	file := File{Name: `quoted "report".pdf`, Path: `/runner/attachments/quoted "report".pdf`, MediaType: "application/pdf", Size: 10}
	text := Prompt(Operation{Text: "Read the report", Files: []File{file}})
	prefix, reference, ok := strings.Cut(text, "\n\n")
	if !ok || prefix != "Read the report" || reference != `Attached file "quoted \"report\".pdf" (application/pdf, 10 B), retained on this Runner at "/runner/attachments/quoted \"report\".pdf": read it as needed.` {
		t.Fatalf("generic file references were not delivered escaped: %q", text)
	}
}

// A provider process that ended, or a worker whose SDK stream failed, leaves
// nothing to take the next message: those are Runner faults, never turn faults.
func TestTurnFaultClassification(t *testing.T) {
	for _, fault := range []string{"provider_error", "provider_failure", "invalid_request", "unknown"} {
		if !TurnFault(fault) {
			t.Errorf("%q not treated as a turn fault", fault)
		}
	}
	for _, fault := range []string{"", "provider_process_exit", "sdk_error", "provider stream ended: EOF", "native_exit_unconfirmed", "provider startup failed", "exhausted", "auth_required"} {
		if TurnFault(fault) {
			t.Errorf("%q treated as a turn fault", fault)
		}
	}
}
