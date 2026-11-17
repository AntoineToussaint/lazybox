package process

import (
	"fmt"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"

	"gopkg.in/yaml.v3"
)

// checkNode validates a parsed YAML file against the generated JSON Schema
// before anything is decoded, so an unknown key, a wrong type or a value off
// an enum is reported with its path and line. It covers exactly the schema
// keywords the generator emits.
func checkNode(root *yaml.Node) []Issue {
	c := checker{defs: schemaTree()["$defs"].(map[string]any)}
	c.check(root, schemaTree(), nil)
	return c.issues
}

type checker struct {
	defs   map[string]any
	issues []Issue
}

func (c *checker) fail(n *yaml.Node, path Path, format string, args ...any) {
	c.issues = append(c.issues, Issue{Severity: Error, Path: path.String(), Line: n.Line, Column: n.Column, Message: fmt.Sprintf(format, args...)})
}

func (c *checker) resolve(s map[string]any) map[string]any {
	for {
		ref, ok := s["$ref"].(string)
		if !ok {
			return s
		}
		s = c.defs[strings.TrimPrefix(ref, "#/$defs/")].(map[string]any)
	}
}

var patterns sync.Map // schema pattern → *regexp.Regexp

func compiled(pattern string) *regexp.Regexp {
	if re, ok := patterns.Load(pattern); ok {
		return re.(*regexp.Regexp)
	}
	re := regexp.MustCompile(pattern) // the generator's own patterns
	patterns.Store(pattern, re)
	return re
}

func nodeType(n *yaml.Node) string {
	switch n.Kind {
	case yaml.MappingNode:
		return "object"
	case yaml.SequenceNode:
		return "array"
	case yaml.ScalarNode:
		switch n.Tag {
		case "!!null":
			return "null"
		case "!!bool":
			return "boolean"
		case "!!int":
			return "integer"
		case "!!float":
			return "number"
		}
		return "string"
	}
	return "unknown"
}

func typeMatches(want, got string) bool {
	return want == got || (want == "number" && got == "integer")
}

func (c *checker) check(n *yaml.Node, s map[string]any, path Path) {
	if n.Kind == yaml.AliasNode {
		n = n.Alias
	}
	s = c.resolve(s)
	if alternatives, ok := s["oneOf"].([]any); ok {
		c.checkOneOf(n, alternatives, path)
		return
	}
	want, _ := s["type"].(string)
	got := nodeType(n)
	if !typeMatches(want, got) {
		c.fail(n, path, "expected %s, found %s", describe(s), describeNode(n))
		return
	}
	switch want {
	case "object":
		c.checkObject(n, s, path)
	case "array":
		if min, ok := s["minItems"].(int); ok && len(n.Content) < min {
			c.fail(n, path, "needs at least %d item(s)", min)
		}
		items := s["items"].(map[string]any)
		for i, item := range n.Content {
			c.check(item, items, path.Index(i))
		}
	case "string":
		if enum, ok := s["enum"].([]string); ok && !slices.Contains(enum, n.Value) {
			c.fail(n, path, "%q is not one of %s", n.Value, strings.Join(enum, ", "))
		}
		if pattern, ok := s["pattern"].(string); ok && !compiled(pattern).MatchString(n.Value) {
			c.fail(n, path, "%q is not a duration such as 30m, 10h or 1h30m", n.Value)
		}
	case "integer", "number":
		value, err := strconv.ParseFloat(n.Value, 64)
		if err != nil {
			c.fail(n, path, "%q is not a number", n.Value)
			return
		}
		if min, ok := s["minimum"].(int); ok && value < float64(min) {
			c.fail(n, path, "%s is below the minimum %d", n.Value, min)
		}
		if max, ok := s["maximum"].(int); ok && value > float64(max) {
			c.fail(n, path, "%s is above the maximum %d", n.Value, max)
		}
	}
}

func (c *checker) checkObject(n *yaml.Node, s map[string]any, path Path) {
	properties, _ := s["properties"].(map[string]any)
	seen := map[string]bool{}
	for i := 0; i+1 < len(n.Content); i += 2 {
		key, value := n.Content[i], n.Content[i+1]
		if seen[key.Value] {
			c.fail(key, path.Key(key.Value), "duplicate key")
			continue
		}
		seen[key.Value] = true
		if property, ok := properties[key.Value].(map[string]any); ok {
			c.check(value, property, path.Key(key.Value))
			continue
		}
		switch extra := s["additionalProperties"].(type) {
		case map[string]any:
			c.check(value, extra, path.Key(key.Value))
		default:
			known := make([]string, 0, len(properties))
			for name := range properties {
				known = append(known, name)
			}
			slices.Sort(known)
			c.fail(key, path.Key(key.Value), "unknown key; expected one of %s", strings.Join(known, ", "))
		}
	}
	required, _ := s["required"].([]string)
	for _, name := range required {
		if !seen[name] {
			c.fail(n, path, "%s is required", name)
		}
	}
}

// checkOneOf reports against the alternative whose type matches the node, so
// a mapping is checked as the object form and a string as the string form.
func (c *checker) checkOneOf(n *yaml.Node, alternatives []any, path Path) {
	got := nodeType(n)
	var described []string
	for _, a := range alternatives {
		s := c.resolve(a.(map[string]any))
		described = append(described, describe(s))
		if want, _ := s["type"].(string); typeMatches(want, got) {
			c.check(n, s, path)
			return
		}
	}
	c.fail(n, path, "expected %s, found %s", strings.Join(described, " or "), describeNode(n))
}

func describe(s map[string]any) string {
	t, _ := s["type"].(string)
	if enum, ok := s["enum"].([]string); ok && len(enum) == 1 {
		return strconv.Quote(enum[0])
	}
	switch t {
	case "object":
		return "a mapping"
	case "array":
		return "a list"
	case "null":
		return "null"
	case "integer":
		return "an integer"
	}
	return "a " + t
}

func describeNode(n *yaml.Node) string {
	switch t := nodeType(n); t {
	case "object":
		return "a mapping"
	case "array":
		return "a list"
	case "null":
		return "null"
	default:
		return fmt.Sprintf("%s %q", t, n.Value)
	}
}
