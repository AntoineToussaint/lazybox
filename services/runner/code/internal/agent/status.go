package agent

import (
	"strings"
	"time"
)

// Status is observed execution state. Done means a provider turn completed;
// it does not accept a work item or close the conversation.
type Status string

const (
	Starting        Status = "starting"
	Ready           Status = "ready"
	Running         Status = "running"
	WaitingInput    Status = "waiting_input"
	WaitingApproval Status = "waiting_approval"
	Done            Status = "done"
	Interrupted     Status = "interrupted"
	Failed          Status = "failed"
	Stopped         Status = "stopped"
	Unknown         Status = "unknown"
	Switching       Status = "switching_profile"
)

type Activity struct {
	ID             string    `json:"id"`
	ParentID       string    `json:"parentId,omitempty"`
	NativeID       string    `json:"nativeId,omitempty"`
	Name           string    `json:"name"`
	Kind           string    `json:"kind"`
	Status         Status    `json:"status"`
	LastTurnStatus string    `json:"lastTurnStatus,omitempty"`
	UpdatedAt      time.Time `json:"updatedAt"`
	// TurnEndedAt is when the agent's last turn ended. Unlike UpdatedAt, no
	// status report or Runner restart moves it.
	TurnEndedAt time.Time `json:"turnEndedAt,omitzero"`
	// TurnStartedAt is when the agent's current or last turn began.
	TurnStartedAt time.Time `json:"turnStartedAt,omitzero"`
	// Phase is what the agent reports doing inside a turn: working,
	// requesting, thinking, responding, compacting or tool. It is empty
	// between turns and while the agent waits for the person.
	Phase string `json:"phase,omitempty"`
	// Tool names the tool during the tool phase, when the provider reports it.
	Tool string `json:"tool,omitempty"`
	// PhaseSince is when Phase or Tool last changed.
	PhaseSince time.Time `json:"phaseSince,omitzero"`
}

// AccountObservation only contains provider-reported account and availability facts, never
// credential material. Context/budget failures are execution limits, not quotas.
type UsageWindow struct {
	Name            string  `json:"name"`
	DurationMinutes int64   `json:"durationMinutes,omitempty"`
	UsedPercent     float64 `json:"usedPercent"`
	ResetAt         int64   `json:"resetAt,omitempty"`
}

// AccountWide reports whether this window can constrain the whole subscription.
func (w UsageWindow) AccountWide() bool {
	return !strings.HasPrefix(w.Name, "model:") && w.Name != "seven_day_sonnet" && w.Name != "seven_day_opus" && w.Name != "seven_day_oauth_apps"
}

type AccountObservation struct {
	Email             string        `json:"email,omitempty"`
	Windows           []UsageWindow `json:"windows,omitempty"`
	Plan              string        `json:"plan,omitempty"`
	ObservedAt        int64         `json:"observedAt,omitempty"`
	State             string        `json:"state"`
	Reason            string        `json:"reason,omitempty"`
	ResetAt           int64         `json:"resetAt,omitempty"`
	RetryAfterSeconds int64         `json:"retryAfterSeconds,omitempty"`
}

func LimitState(code string) string {
	switch code {
	case "usageLimitExceeded", "usage_limit_exceeded", "credits_required", "subscription_exhausted":
		return "exhausted"
	case "rateLimitExceeded", "rate_limit", "rate_limit_error":
		return "throttled"
	case "unauthorized", "authentication_failed", "oauth_org_not_allowed", "cloud_credential_error", "not_logged_in":
		return "auth_required"
	case "contextWindowExceeded", "context_length_exceeded":
		return "context_full"
	case "sessionBudgetExceeded", "error_max_budget_usd", "error_max_turns":
		return "budget_exceeded"
	case "billing_error":
		return "billing_error"
	default:
		return "unknown"
	}
}

func TurnStatus(outcome string) Status {
	switch outcome {
	case "completed", "success":
		return Done
	case "interrupted", "canceled", "cancelled", "stopped":
		return Interrupted
	case "failed", "error":
		return Failed
	default:
		return Unknown
	}
}
