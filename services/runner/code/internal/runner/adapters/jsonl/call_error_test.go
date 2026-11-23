package jsonl

import (
	"encoding/json"
	"errors"
	"fmt"
	"testing"
)

// Regression: a failed model change read `adapter model: {"message":"Error:
// API error: 429 …"}` in the conversation's banner. The person reads the
// agent's own words; the method, code and data stay for code.
func TestAnAdaptersErrorReadsAsItsMessage(t *testing.T) {
	raw := json.RawMessage(`{"code":-32000,"message":"Error: API error: 429 This request would exceed your account's rate limit. Please try again later","data":{"retry":30}}`)
	err := error(NewCallError("model", raw))
	if got := err.Error(); got != "API error: 429 This request would exceed your account's rate limit. Please try again later" {
		t.Fatalf("reads %q", got)
	}
	var call *CallError
	if !errors.As(fmt.Errorf("changing the model: %w", err), &call) || call.Method != "model" || call.Code != -32000 || string(call.Data) != `{"retry":30}` {
		t.Fatalf("code lost what it decides on: %+v", call)
	}
	if got := NewCallError("turn", json.RawMessage(`"Error: busy"`)).Error(); got != "busy" {
		t.Fatalf("a plain string error reads %q", got)
	}
	if got := NewCallError("turn", json.RawMessage(`42`)).Error(); got != "the agent refused turn: 42" {
		t.Fatalf("an answer that is not an error object reads %q", got)
	}
}
