package jsonl

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"testing"
)

// Regression: a failed model change read `adapter model: {"message":"Error:
// API error: 429 …"}` in the conversation's banner. The person reads the
// adapter's own words; the method, code and data stay for code.
func TestAnAdaptersErrorReadsAsItsMessage(t *testing.T) {
	raw := json.RawMessage(`{"code":-32000,"message":"API error: 429 This request would exceed your account's rate limit. Please try again later","data":{"retry":30}}`)
	err := error(NewCallError("model", raw))
	if got := err.Error(); got != "API error: 429 This request would exceed your account's rate limit. Please try again later" {
		t.Fatalf("reads %q", got)
	}
	var call *CallError
	if !errors.As(fmt.Errorf("changing the model: %w", err), &call) || call.Method != "model" || call.Code != -32000 || string(call.Data) != `{"retry":30}` {
		t.Fatalf("code lost what it decides on: %+v", call)
	}
	for name, tc := range map[string]struct{ raw, want string }{
		// ACP's internal errors put the reason in data.
		"generic message, reason in data": {`{"code":-32603,"message":"Internal error","data":{"details":"model claude-x not available"}}`, "Internal error: model claude-x not available"},
		"reason as a string":              {`{"code":-32603,"message":"Internal error","data":"model claude-x not available"}`, "Internal error: model claude-x not available"},
		"data that repeats the message":   {`{"message":"Rate limited","data":{"message":"Rate limited"}}`, "Rate limited"},
		"a code of another type":          {`{"code":"E_RATE","message":"Rate limited"}`, "Rate limited"},
		"a string error":                  {`"busy"`, "busy"},
		"nothing to read":                 {`{"code":-1}`, "turn failed (error -1)"},
		"not an error object":             {`42`, "turn failed"},
		"a bare code":                     {`{"code":-32601}`, "turn failed (error -32601)"},
		"a stack trace in data":           {`{"message":"Internal error","data":"TypeError: x is undefined\n    at a (b.js:1)\n    at c (d.js:2)"}`, "Internal error: TypeError: x is undefined"},
	} {
		if got := NewCallError("turn", json.RawMessage(tc.raw)).Error(); got != tc.want {
			t.Errorf("%s: reads %q, want %q", name, got, tc.want)
		}
	}
}

// A long reason in data is cut to one bounded line: the rest stays in the log.
func TestALongReasonIsOneBoundedLine(t *testing.T) {
	long := strings.Repeat("x", 1000)
	got := NewCallError("turn", json.RawMessage(`{"message":"Internal error","data":"`+long+`"}`)).Error()
	if n := len([]rune(got)); n > len("Internal error: ")+detailLimit || !strings.HasSuffix(got, "…") {
		t.Fatalf("%d runes: %q", n, got[:40])
	}
}
