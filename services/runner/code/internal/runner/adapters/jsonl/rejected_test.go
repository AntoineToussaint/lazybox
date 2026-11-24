package jsonl

import (
	"encoding/json"
	"errors"
	"fmt"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A JSON-RPC error answer is the peer's definite refusal: the engine makes a
// refused message a turn's failure, never a delivery whose outcome is unknown.
func TestACallErrorIsADefiniteRefusal(t *testing.T) {
	err := fmt.Errorf("delivering: %w", NewCallError("turn/start", json.RawMessage(`{"code":-32602,"message":"invalid params"}`)))
	if !errors.Is(err, agent.ErrRejected) {
		t.Fatal("a JSON-RPC error answer is not a refusal")
	}
	if errors.Is(errors.New("connection reset"), agent.ErrRejected) {
		t.Fatal("any error is a refusal")
	}
}
