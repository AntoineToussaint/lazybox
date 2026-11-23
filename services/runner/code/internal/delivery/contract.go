// Package delivery defines the acceptance evidence shared by conversations and Missions.
package delivery

import (
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
)

type Criterion struct {
	ID       string `json:"id"`
	Expected string `json:"expected"`
	Required bool   `json:"required"`
}

var criterionID = regexp.MustCompile(`^[a-z][a-z0-9_-]{0,63}$`)

// ParseSpecV2 validates the complete published document before it can be bound.
func ParseSpecV2(content string) ([]Criterion, error) {
	var spec struct {
		Schema       string   `json:"schema"`
		Problem      string   `json:"problem"`
		Requirements []string `json:"requirements"`
		Criteria     []struct {
			ID       string `json:"id"`
			Expected string `json:"expected"`
			Required *bool  `json:"required"`
		} `json:"acceptanceCriteria"`
	}
	if !json.Valid([]byte(content)) {
		return nil, fmt.Errorf("invalid spec JSON")
	}
	dec := json.NewDecoder(strings.NewReader(content))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&spec); err != nil {
		return nil, err
	}
	if spec.Schema != "spec.v2" || strings.TrimSpace(spec.Problem) == "" || spec.Requirements == nil || len(spec.Criteria) == 0 {
		return nil, fmt.Errorf("spec.v2 needs a problem, requirements and acceptance criteria")
	}
	for _, r := range spec.Requirements {
		if strings.TrimSpace(r) == "" {
			return nil, fmt.Errorf("empty requirement")
		}
	}
	seen := map[string]bool{}
	var criteria []Criterion
	for _, c := range spec.Criteria {
		if !criterionID.MatchString(c.ID) || seen[c.ID] || strings.TrimSpace(c.Expected) == "" || c.Required == nil {
			return nil, fmt.Errorf("invalid or duplicate acceptance criterion %q", c.ID)
		}
		seen[c.ID] = true
		criteria = append(criteria, Criterion{c.ID, c.Expected, *c.Required})
	}
	return criteria, nil
}
