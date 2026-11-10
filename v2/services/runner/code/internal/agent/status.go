package agent

import "time"

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
}

// AccountObservation only contains provider-reported account and availability facts, never
// credential material. Context/budget failures are execution limits, not quotas.
type UsageWindow struct {
	Name            string  `json:"name"`
	DurationMinutes int64   `json:"durationMinutes,omitempty"`
	UsedPercent     float64 `json:"usedPercent"`
	ResetAt         int64   `json:"resetAt,omitempty"`
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

func IsAssistant(purpose string) bool {
	return purpose == "help" || purpose == "planning" || purpose == "summary"
}

func ValidPurpose(purpose string) bool {
	return purpose == "" || purpose == "coding" || IsAssistant(purpose)
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
