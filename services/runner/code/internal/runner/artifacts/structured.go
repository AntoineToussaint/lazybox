package artifacts

import (
	"embed"
	"encoding/json"
	"fmt"
	"slices"
	"strings"
)

//go:embed templates/*.json
var templates embed.FS

// Template returns an editable example for the supported artifact kind.
func Template(kind string) (string, error) {
	if !slices.Contains(Kinds, kind) {
		return "", fmt.Errorf("%w: unknown artifact kind %q", ErrInvalid, kind)
	}
	data, err := templates.ReadFile("templates/" + kind + ".json")
	return string(data), err
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
