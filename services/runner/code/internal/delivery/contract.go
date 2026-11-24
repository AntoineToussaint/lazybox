// Package delivery defines the acceptance evidence shared by conversations and Missions.
package delivery

import (
	"encoding/json"
	"fmt"
	"regexp"
	"slices"
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

// ContractFromSpec pins one published spec revision as a contract. A spec.v2
// revision binds named criteria; a spec.v1 revision keeps its text criteria unbound.
func ContractFromSpec(artifact string, version uint32, digest, content string) (Contract, error) {
	var header struct {
		Schema             string          `json:"schema"`
		AcceptanceCriteria json.RawMessage `json:"acceptanceCriteria"`
	}
	if err := json.Unmarshal([]byte(content), &header); err != nil {
		return Contract{}, fmt.Errorf("the contract needs a structured spec")
	}
	c := Contract{Artifact: artifact, Version: version, Schema: header.Schema, Digest: digest}
	switch header.Schema {
	case "spec.v2":
		criteria, err := ParseSpecV2(content)
		if err != nil {
			return Contract{}, err
		}
		c.Criteria = criteria
	case "spec.v1":
		if json.Unmarshal(header.AcceptanceCriteria, &c.Legacy) != nil || len(c.Legacy) == 0 {
			return Contract{}, fmt.Errorf("missing acceptance criteria")
		}
	default:
		return Contract{}, fmt.Errorf("unsupported spec schema %q", header.Schema)
	}
	return c, nil
}

// Lines is the contract's criteria as a person and an agent read them:
// "[id, required] expected" when bound, the original text otherwise.
func (c Contract) Lines() []string {
	if !c.Bound() {
		return slices.Clone(c.Legacy)
	}
	lines := make([]string, 0, len(c.Criteria))
	for _, criterion := range c.Criteria {
		level := "optional"
		if criterion.Required {
			level = "required"
		}
		lines = append(lines, fmt.Sprintf("[%s, %s] %s", criterion.ID, level, criterion.Expected))
	}
	return lines
}
