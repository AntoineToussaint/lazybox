package process

import (
	"cmp"
	"fmt"
	"slices"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
)

// Severity of an Issue. Only errors make a process invalid.
type Severity string

const (
	Error   Severity = "error"
	Warning Severity = "warning"
)

// Issue is one finding about a process, located as precisely as the layers
// allow: the key path in the effective process (roles.reviewer.triggers[0].on)
// and the layer, file and line that wrote it.
type Issue struct {
	Severity Severity `json:"severity"`
	Path     string   `json:"path,omitempty"`
	Message  string   `json:"message"`
	// Layer is the layer that set the value: "preset:<name>", "user" or
	// "repository"; empty when the problem is the combination.
	Layer  string `json:"layer,omitempty"`
	Source string `json:"source,omitempty"`
	Line   int    `json:"line,omitempty"`
	Column int    `json:"column,omitempty"`
	path   Path
}

func (i Issue) String() string {
	var b strings.Builder
	if i.Source != "" {
		b.WriteString(i.Source)
		if i.Line > 0 {
			fmt.Fprintf(&b, ":%d:%d", i.Line, i.Column)
		}
		b.WriteString(": ")
	}
	if i.Severity == Warning {
		b.WriteString("warning: ")
	}
	if i.Path != "" {
		b.WriteString(i.Path)
		b.WriteString(": ")
	}
	b.WriteString(i.Message)
	return b.String()
}

// Issues is a list of findings.
type Issues []Issue

// Errors returns only the errors.
func (l Issues) Errors() Issues {
	var out Issues
	for _, i := range l {
		if i.Severity == Error {
			out = append(out, i)
		}
	}
	return out
}

// sort puts errors before warnings, each in file and line order.
func (l Issues) sort() {
	slices.SortStableFunc(l, func(a, b Issue) int {
		if a.Severity != b.Severity {
			if a.Severity == Error {
				return -1
			}
			return 1
		}
		return cmp.Or(cmp.Compare(a.Source, b.Source), cmp.Compare(a.Line, b.Line), cmp.Compare(a.Column, b.Column))
	})
}

// Err returns nil when there is no error, else one error listing them all.
func (l Issues) Err() error {
	errs := l.Errors()
	if len(errs) == 0 {
		return nil
	}
	lines := make([]string, len(errs))
	for i, e := range errs {
		lines[i] = e.String()
	}
	return fmt.Errorf("invalid process:\n  %s", strings.Join(lines, "\n  "))
}

// Path addresses a value in a process: keys and list indexes.
type Path []any

func (p Path) Key(k string) Path { return append(p[:len(p):len(p)], k) }
func (p Path) Index(i int) Path  { return append(p[:len(p):len(p)], i) }

func (p Path) String() string {
	var b strings.Builder
	for _, part := range p {
		switch v := part.(type) {
		case int:
			b.WriteString("[" + strconv.Itoa(v) + "]")
		case string:
			if b.Len() > 0 {
				b.WriteByte('.')
			}
			b.WriteString(v)
		}
	}
	return b.String()
}

// lookup returns the node at path in a parsed file and how many parts of the
// path it found (len(path) when the whole path exists).
func lookup(n *yaml.Node, path Path) (*yaml.Node, int) {
	if n != nil && n.Kind == yaml.DocumentNode && len(n.Content) > 0 {
		n = n.Content[0]
	}
	if n == nil {
		return nil, 0
	}
	for depth, part := range path {
		if n.Kind == yaml.AliasNode {
			n = n.Alias
		}
		var next *yaml.Node
		switch v := part.(type) {
		case string:
			if n.Kind == yaml.MappingNode {
				for i := 0; i+1 < len(n.Content); i += 2 {
					if n.Content[i].Value == v {
						next = n.Content[i+1]
						break
					}
				}
			}
		case int:
			if n.Kind == yaml.SequenceNode && v < len(n.Content) {
				next = n.Content[v]
			}
		}
		if next == nil {
			return n, depth
		}
		n = next
	}
	return n, len(path)
}
