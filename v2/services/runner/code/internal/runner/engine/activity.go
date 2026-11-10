package engine

import (
	"sort"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
)

func (e *Engine) observe(ev agent.Event) {
	id := ev.AgentID
	if id == "" {
		id = "root"
	}
	a, exists := e.state.Agents[id]
	if !exists {
		switch ev.Type {
		case "agent_status", "turn_started", "turn_ended", "question", "approval", "failure":
		default:
			return
		}
		if len(e.state.Agents) >= 256 {
			e.state.ActivityTruncated = true
			return
		}
		a = agent.Activity{ID: id, Name: "Agent", Kind: "agent", Status: agent.Unknown}
		if id != "root" {
			a.ParentID = ev.ParentAgentID
			if a.ParentID == "" {
				a.ParentID = "root"
			}
		}
	}
	if ev.Name != "" {
		a.Name = ev.Name
	}
	if ev.AgentKind != "" {
		a.Kind = ev.AgentKind
	}
	if ev.NativeID != "" {
		a.NativeID = ev.NativeID
	}
	if ev.ParentAgentID != "" && id != "root" && ev.ParentAgentID != id {
		a.ParentID = ev.ParentAgentID
	}
	switch ev.Type {
	case "session_ready":
		a.Status = agent.Ready
	case "turn_started":
		a.Status = agent.Running
	case "turn_ended":
		a.Status, a.LastTurnStatus = agent.TurnStatus(ev.Status), ev.Status
	case "question":
		a.Status = agent.WaitingInput
	case "approval":
		a.Status = agent.WaitingApproval
	case "request_resolved":
		a.Status = agent.Running
	case "agent_status":
		switch agent.Status(ev.Status) {
		case agent.Starting, agent.Ready, agent.Running, agent.WaitingInput, agent.WaitingApproval, agent.Done, agent.Interrupted, agent.Failed, agent.Stopped, agent.Unknown:
			if ev.Status != "ready" || (a.Status != agent.Done && a.Status != agent.Interrupted) {
				a.Status = agent.Status(ev.Status)
			}
		default:
			a.Status = agent.Unknown
		}
	case "profile_switching":
		for id, child := range e.state.Agents {
			if id != "root" {
				child.Status = agent.Stopped
				e.state.Agents[id] = child
			}
		}
		a.Status = agent.Switching
	case "profile_changed":
		if id == "root" {
			e.state.Profile = ev.Text
			e.state.Reference = ev.SessionID
			e.state.Fault = ""
			e.state.Account = &agent.AccountObservation{State: "unknown"}
			a.Status = agent.Starting
		}
	case "failure":
		a.Status = agent.Failed
	case "process_exit":
		for id, child := range e.state.Agents {
			if child.Status == agent.Running || child.Status == agent.Starting || child.Status == agent.WaitingInput || child.Status == agent.WaitingApproval {
				child.Status = agent.Unknown
				e.state.Agents[id] = child
			}
		}
		a.Status = agent.Unknown
	case "session_closed":
		for id, child := range e.state.Agents {
			child.Status = agent.Stopped
			child.UpdatedAt = time.Now().UTC()
			e.state.Agents[id] = child
		}
		a.Status = agent.Stopped
	}
	a.UpdatedAt = time.Now().UTC()
	e.state.Agents[id] = a
	if ev.Account != nil && (ev.Account.State != "unknown" || e.state.Account == nil || e.state.Account.State == "unknown") {
		observation := *ev.Account
		e.state.Account = &observation
	}
}

func snapshotStatus(s Snapshot) agent.Status {
	if s.Closed {
		if s.Fault == "native_exit_unconfirmed" {
			return agent.Unknown
		}
		return agent.Stopped
	}
	root := s.Agents["root"].Status
	if root == agent.Switching {
		return root
	}
	if s.Fault != "" {
		if root == agent.Unknown {
			return agent.Unknown
		}
		return agent.Failed
	}
	for _, pending := range s.Pending {
		if pending.Type == "question" {
			return agent.WaitingInput
		}
	}
	if len(s.Pending) > 0 {
		return agent.WaitingApproval
	}
	for id, child := range s.Agents {
		if id != "root" && (child.Status == agent.Running || child.Status == agent.Starting || child.Status == agent.WaitingApproval || child.Status == agent.WaitingInput) {
			return agent.Running
		}
	}
	return root
}

func SortedActivities(values map[string]agent.Activity) []agent.Activity {
	items := make([]agent.Activity, 0, len(values))
	for _, value := range values {
		items = append(items, value)
	}
	sort.Slice(items, func(i, j int) bool {
		if items[i].ID == "root" {
			return items[j].ID != "root"
		}
		if items[j].ID == "root" {
			return false
		}
		return items[i].ID < items[j].ID
	})
	return items
}
