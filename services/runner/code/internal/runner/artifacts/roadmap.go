package artifacts

import (
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
)

type roadmap struct {
	Schema     string      `json:"schema"`
	Milestones []milestone `json:"milestones"`
}

type milestone struct {
	ID               string   `json:"id"`
	Title            string   `json:"title"`
	Owner            string   `json:"owner,omitempty"`
	Phase            *string  `json:"phase"`
	Status           string   `json:"status"`
	Deliverable      *string  `json:"deliverable"`
	DependsOn        []string `json:"dependsOn"`
	DefinitionOfDone []string `json:"definitionOfDone"`
	Evidence         []string `json:"evidence"`
	OpenDecisions    []string `json:"openDecisions"`
}

var milestoneID = regexp.MustCompile("^[A-Za-z0-9][A-Za-z0-9_-]*$")

func validateRoadmap(content string) error {
	var plan roadmap
	decoder := json.NewDecoder(strings.NewReader(content))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&plan); err != nil || len(plan.Milestones) == 0 {
		return fmt.Errorf("%w: expected a roadmap with milestones", ErrInvalid)
	}
	dependencies := make(map[string][]string)
	for _, m := range plan.Milestones {
		if _, exists := dependencies[m.ID]; exists || !milestoneID.MatchString(m.ID) || strings.TrimSpace(m.Title) == "" {
			return fmt.Errorf("%w: each milestone needs a unique ID and title", ErrInvalid)
		}
		if m.Phase == nil || m.Deliverable == nil {
			return fmt.Errorf("%w: milestone %s needs phase and deliverable fields", ErrInvalid, m.ID)
		}
		switch m.Status {
		case "open", "in-progress", "blocked", "awaiting-acceptance", "accepted":
		default:
			return fmt.Errorf("%w: unknown milestone status %q", ErrInvalid, m.Status)
		}
		for _, entries := range [][]string{m.DependsOn, m.DefinitionOfDone, m.Evidence, m.OpenDecisions} {
			if entries == nil {
				return fmt.Errorf("%w: milestone %s requires list fields", ErrInvalid, m.ID)
			}
			for _, entry := range entries {
				if strings.TrimSpace(entry) == "" {
					return fmt.Errorf("%w: empty list entry in %s", ErrInvalid, m.ID)
				}
			}
		}
		if m.Status == "accepted" && (len(m.DefinitionOfDone) == 0 || len(m.Evidence) == 0) {
			return fmt.Errorf("%w: accepted milestone %s needs criteria and evidence", ErrInvalid, m.ID)
		}
		dependencies[m.ID] = m.DependsOn
	}
	visited := map[string]bool{}
	var visit func(string) error
	visit = func(id string) error {
		deps, exists := dependencies[id]
		if !exists {
			return fmt.Errorf("%w: unknown dependency %s", ErrInvalid, id)
		}
		if complete, seen := visited[id]; seen {
			if !complete {
				return fmt.Errorf("%w: dependency cycle at %s", ErrInvalid, id)
			}
			return nil
		}
		visited[id] = false
		for _, dep := range deps {
			if err := visit(dep); err != nil {
				return err
			}
		}
		visited[id] = true
		return nil
	}
	for id := range dependencies {
		if err := visit(id); err != nil {
			return err
		}
	}
	return nil
}
