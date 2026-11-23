package artifacts

import (
	"embed"
	"encoding/json"
	"fmt"
	"slices"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
)

//go:embed templates/*.json
var templates embed.FS

var starterNames = []string{"pr-walkthrough", "incident-timeline", "release-checklist", "roadmap", "delivery-spec"}

// Template returns an editable artifact example by kind or named starter.
func Template(kind string) (string, error) {
	if !slices.Contains(Kinds, kind) && !slices.Contains(starterNames, kind) {
		return "", fmt.Errorf("%w: unknown artifact kind %q", ErrInvalid, kind)
	}
	data, err := templates.ReadFile("templates/" + kind + ".json")
	return string(data), err
}

// structuredSchema names the schema a validated body declares, such as spec.v2.
func structuredSchema(kind, content string) string {
	var header struct{ Schema string }
	if json.Unmarshal([]byte(content), &header) != nil || header.Schema == "" {
		return kind + ".v1"
	}
	return header.Schema
}

func validateStructured(kind, content string) error {
	if kind == "todo-list" {
		return validateTodoList(content)
	}
	example, err := Template(kind)
	if err != nil {
		return err
	}
	var fields map[string]json.RawMessage
	if err = json.Unmarshal([]byte(example), &fields); err != nil {
		return err
	}
	var body map[string]json.RawMessage
	if err = json.Unmarshal([]byte(content), &body); err != nil {
		return fmt.Errorf("%w: expected a JSON object", ErrInvalid)
	}
	var schema string
	_ = json.Unmarshal(body["schema"], &schema)
	switch {
	case kind == "plan" && schema == "roadmap.v1":
		return validateRoadmap(content)
	case kind == "spec" && schema == "spec.v2":
		if _, err := delivery.ParseSpecV2(content); err != nil {
			return fmt.Errorf("%w: %v", ErrInvalid, err)
		}
		return nil
	}
	for name, shape := range fields {
		raw := body[name]
		if shape[0] == '[' {
			var rows []string
			if json.Unmarshal(raw, &rows) != nil || rows == nil {
				return fmt.Errorf("%w: %s needs a list of text entries", ErrInvalid, name)
			}
			for _, row := range rows {
				if strings.TrimSpace(row) == "" {
					return fmt.Errorf("%w: blank entry in %s", ErrInvalid, name)
				}
			}
		} else {
			var value string
			if json.Unmarshal(raw, &value) != nil || strings.TrimSpace(value) == "" {
				return fmt.Errorf("%w: %s needs text", ErrInvalid, name)
			}
			if name == "schema" && value != kind+".v1" {
				return fmt.Errorf("%w: expected schema %s.v1", ErrInvalid, kind)
			}
		}
	}
	for name := range body {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("%w: unknown %s field %s", ErrInvalid, kind, name)
		}
	}
	return nil
}
