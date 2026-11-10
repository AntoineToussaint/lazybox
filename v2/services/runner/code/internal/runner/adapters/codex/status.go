package codex

import (
	"encoding/json"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
)

func actorID(native, root string) string {
	if native == "" || native == root {
		return "root"
	}
	return "codex:" + native
}
func errorObservation(raw json.RawMessage) *agent.AccountObservation {
	var detail struct {
		Code json.RawMessage `json:"codexErrorInfo"`
	}
	_ = json.Unmarshal(raw, &detail)
	var code string
	if json.Unmarshal(detail.Code, &code) != nil {
		var tagged map[string]json.RawMessage
		_ = json.Unmarshal(detail.Code, &tagged)
		if len(tagged) == 1 {
			for key, payload := range tagged {
				code = key
				var http struct {
					Status int `json:"httpStatusCode"`
				}
				_ = json.Unmarshal(payload, &http)
				if http.Status == 401 {
					code = "unauthorized"
				} else if http.Status == 429 {
					code = "rateLimitExceeded"
				}
			}
		}
	}
	return &agent.AccountObservation{State: agent.LimitState(code), Reason: code}
}
func collabStatus(status string) string {
	switch status {
	case "pendingInit":
		return "starting"
	case "running":
		return "running"
	case "completed":
		return "done"
	case "interrupted":
		return "interrupted"
	case "errored":
		return "failed"
	case "shutdown":
		return "stopped"
	default:
		return "unknown"
	}
}

// Native collaboration states, rather than tool completion, describe the child.
func collaborationEvents(raw json.RawMessage, root string) []agent.Event {
	var p struct {
		Item struct {
			Type, SenderThreadID string
			AgentsStates         map[string]struct{ Status string }
		}
	}
	if json.Unmarshal(raw, &p) != nil || p.Item.Type != "collabAgentToolCall" {
		return nil
	}
	events := []agent.Event{}
	for id, state := range p.Item.AgentsStates {
		if id == root {
			continue
		}
		events = append(events, agent.Event{Type: "agent_status", AgentID: actorID(id, root), ParentAgentID: actorID(p.Item.SenderThreadID, root), NativeID: id, AgentKind: "agent", Status: collabStatus(state.Status)})
	}
	return events
}
func threadStatus(raw json.RawMessage) string {
	var p struct {
		Status struct {
			Type        string
			ActiveFlags []string
		}
	}
	_ = json.Unmarshal(raw, &p)
	switch p.Status.Type {
	case "active":
		for _, flag := range p.Status.ActiveFlags {
			if flag == "waitingOnUserInput" {
				return "waiting_input"
			}
			if flag == "waitingOnApproval" {
				return "waiting_approval"
			}
		}
		return "running"
	case "idle":
		return "ready"
	case "systemError":
		return "failed"
	default:
		return "unknown"
	}
}

type quotaWindow struct {
	UsedPercent                  float64
	ResetsAt, WindowDurationMins int64
}
type quotaBucket struct {
	Primary, Secondary  *quotaWindow
	SpendControlReached *bool
	PlanType            string
}

func rateLimits(raw json.RawMessage) *agent.AccountObservation {
	var p struct {
		RateLimits           quotaBucket
		RateLimitsByLimitID  map[string]quotaBucket `json:"rateLimitsByLimitId"`
		OrdinaryUsageAllowed *bool
	}
	if json.Unmarshal(raw, &p) != nil {
		return &agent.AccountObservation{State: "unknown"}
	}
	result := &agent.AccountObservation{State: "unknown", Reason: "account/rateLimits/updated"}
	buckets := p.RateLimitsByLimitID
	if len(buckets) == 0 {
		buckets = map[string]quotaBucket{"codex": p.RateLimits}
	}
	for id, bucket := range buckets {
		if bucket.PlanType != "" {
			result.Plan = bucket.PlanType
		}
		for name, window := range map[string]*quotaWindow{"primary": bucket.Primary, "secondary": bucket.Secondary} {
			if window == nil || window.UsedPercent < 0 {
				continue
			}
			if result.State == "unknown" {
				result.State = "available"
			}
			result.Windows = append(result.Windows, agent.UsageWindow{Name: id + ":" + name, UsedPercent: window.UsedPercent, ResetAt: window.ResetsAt, DurationMinutes: window.WindowDurationMins})
			if window.UsedPercent >= 100 {
				result.State = "exhausted"
				if window.ResetsAt > result.ResetAt {
					result.ResetAt = window.ResetsAt
				}
			}
		}
		if bucket.SpendControlReached != nil && *bucket.SpendControlReached {
			result.State = "exhausted"
			result.ResetAt = 0
			result.Reason = "spend_control"
		}
	}
	if p.OrdinaryUsageAllowed != nil {
		if *p.OrdinaryUsageAllowed {
			result.State = "available"
		} else {
			result.State = "exhausted"
		}
	}
	return result
}
