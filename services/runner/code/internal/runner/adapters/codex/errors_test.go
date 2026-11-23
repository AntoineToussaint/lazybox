package codex

import (
	"encoding/json"
	"errors"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

// Codex not starting names the step it refused; a failure of its own made of
// several (a fork at the wrong turn, then an archive refused) is not blamed
// on the last step.
func TestCodexNamesTheStepItRefused(t *testing.T) {
	refused := jsonl.NewCallError("thread/resume", json.RawMessage(`{"message":"no rollout found"}`))
	if got := startError(refused).Error(); got != "Codex did not start (thread/resume): no rollout found" {
		t.Fatalf("a refusal: %q", got)
	}
	joined := errors.Join(errors.New("Codex fork did not end at the selected turn"), jsonl.NewCallError("thread/archive", json.RawMessage(`{"message":"busy"}`)))
	bare := jsonl.NewCallError("thread/start", json.RawMessage(`{"code":-32600}`))
	if got := startError(bare).Error(); got != "Codex did not start: thread/start failed with no reason given (error -32600)" {
		t.Fatalf("a refusal without a reason names the step once: %q", got)
	}
	if got := startError(joined); got != joined {
		t.Fatalf("a joined failure was renamed: %q", got)
	}
}

// Codex's sign-in refusals are recognised by phrases anywhere in its answer,
// not only in the message the person reads.
func TestCodexSignInRefusalsAreRecognisedInItsWholeAnswer(t *testing.T) {
	device := jsonl.NewCallError("account/login/start", json.RawMessage(`{"message":"login failed","data":{"hint":"Enable device code sign-in in ChatGPT Security Settings"}}`))
	if loginError(jsonl.FullText(device)) != ErrDeviceAuthDisabled {
		t.Fatal("device-code sign-in off, said in data, not recognised")
	}
	port := jsonl.NewCallError("account/login/start", json.RawMessage(`{"message":"failed to start login server: Port 127.0.0.1:1457 is already in use"}`))
	if loginError(jsonl.FullText(port)) != ErrSignInInProgress {
		t.Fatal("a sign-in already in progress not recognised")
	}
}
