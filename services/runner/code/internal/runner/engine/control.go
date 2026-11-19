package engine

import (
	"context"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// controlDeadline bounds a control call to the provider (a stop, an answer, a
// permission or model change, a steer): a provider that never answers must
// not hold the conversation, or the operations waiting behind it, forever.
// A message's delivery is not bounded: its outcome would be unknown.
var controlDeadline = 30 * time.Second

// control asks the provider to apply op within controlDeadline.
func (e *Engine) control(op agent.Operation) (agent.Receipt, error) {
	ctx, cancel := context.WithTimeout(e.ctx, controlDeadline)
	defer cancel()
	return e.provider.Apply(ctx, op)
}

// unblockedByProviderFault reports whether an operation goes on after the
// provider ended a turn with an error or a subscription reached its limit
// (agent.TurnFault, agent.LimitFault): the next message continues the
// conversation, and the person can still stop the agent and answer what it
// asked. Only the Runner's own faults hold them.
func unblockedByProviderFault(kind agent.Kind) bool {
	switch kind {
	case agent.Message, agent.Model, agent.Interrupt, agent.Answer, agent.Approve, agent.RevokeGrant:
		return true
	}
	return false
}
