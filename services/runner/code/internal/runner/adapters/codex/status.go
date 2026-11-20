package codex

import (
	"encoding/json"
	"sort"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
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
// A spawn also names the child after its brief and links it to the spawning
// call, so a client can show the sub-agent where the parent started it.
func collaborationEvents(raw json.RawMessage, root string) []agent.Event {
	var p struct {
		Item struct {
			ID, Type, Tool, SenderThreadID string
			Prompt                         *string
			ReceiverThreadIDs              []string
			AgentsStates                   map[string]struct {
				Status  string
				Message *string
			}
		}
	}
	if json.Unmarshal(raw, &p) != nil || p.Item.Type != "collabAgentToolCall" {
		return nil
	}
	// Only a spawn's prompt is the child's brief; a later sendInput is a message to it.
	brief := ""
	spawn := p.Item.Tool == "" || p.Item.Tool == "spawnAgent"
	if p.Item.Prompt != nil && spawn {
		brief = boundedRunes(*p.Item.Prompt, briefLimit)
	}
	children := make(map[string]bool)
	if spawn {
		for _, id := range p.Item.ReceiverThreadIDs {
			children[id] = true
		}
	}
	for id := range p.Item.AgentsStates {
		children[id] = true
	}
	ids := make([]string, 0, len(children))
	for id := range children {
		if id != "" && id != root {
			ids = append(ids, id)
		}
	}
	sort.Strings(ids)
	events := []agent.Event{}
	for _, id := range ids {
		state, reported := p.Item.AgentsStates[id]
		if !reported {
			state.Status = "pendingInit"
		}
		ev := agent.Event{Type: "agent_status", AgentID: actorID(id, root), ParentAgentID: actorID(p.Item.SenderThreadID, root), NativeID: id, AgentKind: "agent", Status: collabStatus(state.Status)}
		data := map[string]string{}
		if p.Item.ID != "" {
			data["toolUseId"] = p.Item.ID
		}
		if brief != "" {
			data["prompt"] = brief
			ev.Name = briefName(brief)
		}
		if state.Message != nil && *state.Message != "" {
			data["message"] = boundedRunes(*state.Message, briefLimit)
		}
		if len(data) > 0 {
			ev.Data, _ = json.Marshal(data)
		}
		events = append(events, ev)
	}
	return events
}

// briefLimit bounds a sub-agent's brief or message as journaled for display.
const briefLimit = 8000

func boundedRunes(text string, limit int) string {
	runes := []rune(text)
	if len(runes) <= limit {
		return text
	}
	return string(runes[:limit]) + "…"
}

// briefName is the first line of a brief, short enough for a row.
func briefName(brief string) string {
	line, _, _ := strings.Cut(strings.TrimSpace(brief), "\n")
	line = strings.TrimSpace(line)
	if runes := []rune(line); len(runes) > 80 {
		line = string(runes[:79]) + "…"
	}
	return line
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
