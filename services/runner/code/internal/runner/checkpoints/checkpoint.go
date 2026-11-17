package checkpoints

import (
	"encoding/json"
	"errors"
)

// Checkpoint is an explicit handoff of work evidence, never a claim to restore
// private model state or pending provider approvals.
type Checkpoint struct {
	Objective        string   `json:"objective"`
	Criteria         []string `json:"criteria"`
	Decisions        []string `json:"decisions"`
	Outstanding      []string `json:"outstanding"`
	Results          []string `json:"results"`
	SourceRevisions  []string `json:"sourceRevisions"`
	WorkingTreeState string   `json:"workingTreeState"`
	CommandIDs       []string `json:"commandIds"`
	Services         []string `json:"services"`
	OmittedContext   []string `json:"omittedContext"`
}

func (c Checkpoint) Prompt() (string, error) {
	if c.Objective == "" || len(c.Criteria) == 0 || len(c.SourceRevisions) == 0 || c.WorkingTreeState == "" {
		return "", errors.New("checkpoint needs objective, criteria, source revisions, and inspected working-tree state")
	}
	b, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return "", err
	}
	if len(b) > 128<<10 {
		return "", errors.New("checkpoint exceeds 128 KiB")
	}
	return "Continue this work in the existing working directory from the explicit checkpoint below. Inspect current files and retained evidence before acting. Do not repeat completed edits, command starts, or external side effects. Old pending approvals are not transferable. This is a handoff, not an exact conversation restore.\n" + string(b), nil
}
