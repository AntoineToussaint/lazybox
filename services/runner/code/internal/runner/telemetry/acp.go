package telemetry

import (
	"context"
	"math"
	"strings"
)

// reportedUsage records an ACP agent's usage_update: the context it uses of
// its window, and the session's cost when the agent reports one in US
// dollars. The cost is the agent's own cumulative figure; the store records
// its changes within each attempt; Lazybox does not price ACP
// agents' tokens, which ACP does not report.
func (s *session) reportedUsage(ctx context.Context, actor string, body map[string]any) error {
	c := ContextSnapshot{Scope: s.scope, Agent: actor, Source: "acp", Used: count(body["used"]), Capacity: count(body["size"])}
	if err := s.adapter.Store.PutContext(ctx, c); err != nil {
		return err
	}
	cost := object(body["cost"])
	amount, ok := cost["amount"].(float64)
	currency, _ := cost["currency"].(string)
	if !ok || !strings.EqualFold(currency, "USD") || amount < 0 || amount > 1e6 || math.IsNaN(amount) {
		return nil
	}
	nanos := int64(math.Round(amount * 1e9))
	r := Record{ID: "acp-cost:" + s.scope.Execution + "/" + s.scope.Attempt + "/" + actor, Scope: s.scope, Agent: actor, Source: "reported", Status: "complete", Nanos: &nanos, Reason: "Cost reported by the agent"}
	return s.adapter.Store.Put(ctx, r)
}
