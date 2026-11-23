package acp

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

// A turn the agent ended with an error reads as what it said, or as a
// sentence naming the agent when it said nothing readable; a sign-in it
// needs says so either way.
func TestAFailedTurnReadsAsTheAgentsWords(t *testing.T) {
	for name, tc := range map[string]struct {
		failure, text, status string
	}{
		"a message":                        {`{"code":-32603,"message":"Internal error","data":{"details":"model claude-x not available"}}`, "Internal error: model claude-x not available", "provider_error"},
		"data without a reason in words":   {`{"code":-32603,"data":{"trace":[1,2]}}`, "Gemini CLI ended the turn with an error.", "provider_error"},
		"a sign-in, said":                  {`{"code":-32000,"message":"Authentication required"}`, "Authentication required", "auth_required"},
		"a sign-in, with only its methods": {`{"code":-32000,"data":{"authMethods":["oauth"]}}`, "Gemini CLI needs you to sign in again.", "auth_required"},
	} {
		events := turnEnd("t1", nil, json.RawMessage(tc.failure), "Gemini CLI")
		if len(events) != 2 || events[0].Text != tc.text || events[0].Status != tc.status || events[1].Status != "failed" {
			t.Errorf("%s: %+v", name, events)
		}
	}
}

// An agent that did not start says which step refused, in its words, or
// with the error code when it gave no reason.
func TestAnAgentThatDidNotStartSaysWhy(t *testing.T) {
	call := func(raw string) error { return jsonl.NewCallError("initialize", json.RawMessage(raw)) }
	if got := startupError("Gemini CLI", "initialize", call(`{"code":-32601}`)).Error(); got != "Gemini CLI did not start (initialize): error -32601" {
		t.Fatalf("no reason: %q", got)
	}
	if got := startupError("Gemini CLI", "initialize", call(`{"code":-32603,"message":"Internal error","data":"no model"}`)).Error(); got != "Gemini CLI did not start (initialize): Internal error: no model" {
		t.Fatalf("a reason: %q", got)
	}
	if got := startupError("Gemini CLI", "session/new", call(`{"code":-32000,"message":"Authentication required"}`), authMethod{Name: "Google"}).Error(); !strings.HasPrefix(got, "Gemini CLI needs you to sign in.") || !strings.Contains(got, "It signs in with: Google.") {
		t.Fatalf("a sign-in: %q", got)
	}
}
