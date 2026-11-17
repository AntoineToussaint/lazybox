package power

import (
	"errors"
	"fmt"
	"math"
	"regexp"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
)

// layouts/<id>.yaml: one whole-window layout each, offered after the presets
// in the Layout menu, the palette and ⌘⇧L (POWER.md, "Config files").
//
//	name: Review wide
//	description: Changes and the file side by side
//	layout:
//	  split: row                 # row (side by side) or column (stacked)
//	  sizes: [20, 40, 40]        # shares, normalized; equal when left out
//	  children:
//	    - split: column
//	      children: [{panels: [collections]}, {panels: [agents]}]
//	    - panels: [changes]
//	    - panels: [editor, chat] # one group with two tabs
//	      minimized: false
//
// The panels are the app's panel kinds (layoutPanels in the manifest). A
// layout with any error is not offered; the last version that loaded stays.

// Limits of one layout, as the app's own (layout-presets.ts).
const (
	maxLayoutDepth    = 8
	maxLayoutChildren = 16
	maxGroupPanels    = 16
	// MaxLayouts bounds the files read from layouts/.
	MaxLayouts = 64
)

// LayoutIDPattern is a layout file's name without .yaml.
var LayoutIDPattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,63}$`)

// Layout is one layout file, in the app's template shape (TemplateNode).
type Layout struct {
	Name        string     `json:"name"`
	Description string     `json:"description"`
	Layout      LayoutNode `json:"layout"`
}

// LayoutNode is a group of panels or a split of nodes.
type LayoutNode struct {
	Type      string       `json:"type"`
	Kinds     []string     `json:"kinds,omitempty"`
	Minimized bool         `json:"minimized,omitempty"`
	Direction string       `json:"direction,omitempty"`
	Sizes     []float64    `json:"sizes,omitempty"`
	Children  []LayoutNode `json:"children,omitempty"`
}

// errLayoutInvalid: the layout has errors and is not offered.
var errLayoutInvalid = errors.New("the layout has errors; it is not offered until they are fixed")

// ParseLayout reads one layout file.
func ParseLayout(data []byte) (Layout, []Issue, error) {
	root, err := parseRoot(data, "a layout is a mapping with name, description and layout")
	if err != nil {
		return Layout{}, nil, err
	}
	if root == nil {
		return Layout{}, []Issue{{Path: "layout", Message: "the file is empty; a layout needs name and layout", Severity: "error"}}, errLayoutInvalid
	}
	p := &parser{}
	out := Layout{}
	var tree *yaml.Node
	for key, value := range pairs(root) {
		switch key.Value {
		case "version":
			if value.Value != "1" {
				p.fail(value, "version", "only version 1 exists")
			}
		case "name":
			out.Name = strings.TrimSpace(p.str(value, "name", 48))
		case "description":
			out.Description = p.str(value, "description", 120)
		case "layout":
			tree = value
		default:
			p.fail(key, key.Value, "unknown field %q (expected name, description, layout)%s", key.Value, didYouMean(key.Value, []string{"name", "description", "layout"}))
		}
	}
	if out.Name == "" {
		p.fail(root, "name", "a layout needs a name, as the Layout menu shows it")
	}
	if tree == nil {
		p.fail(root, "layout", "a layout needs layout: a group ({panels: [chat]}) or a split ({split: row, children: […]})")
	} else {
		out.Layout = p.layoutNode(tree, "layout", 0)
	}
	if p.failed {
		return out, p.issues, errLayoutInvalid
	}
	return out, p.issues, nil
}

func (p *parser) layoutNode(n *yaml.Node, path string, depth int) LayoutNode {
	if depth > maxLayoutDepth {
		p.fail(n, path, "a layout nests at most %d splits", maxLayoutDepth)
		return LayoutNode{}
	}
	if n.Kind != yaml.MappingNode {
		p.fail(n, path, "each part is a group ({panels: [chat]}) or a split ({split: row, children: […]})")
		return LayoutNode{}
	}
	var panels, split, sizes, children, minimized *yaml.Node
	for key, value := range pairs(n) {
		switch key.Value {
		case "panels":
			panels = value
		case "split":
			split = value
		case "sizes":
			sizes = value
		case "children":
			children = value
		case "minimized":
			minimized = value
		default:
			p.fail(key, path+"."+key.Value, "unknown field %q (a group has panels and minimized; a split has split, sizes and children)%s", key.Value, didYouMean(key.Value, []string{"panels", "split", "sizes", "children", "minimized"}))
		}
	}
	switch {
	case panels != nil && split != nil:
		p.fail(n, path, "a part is either a group (panels) or a split (split, children), not both")
		return LayoutNode{}
	case panels != nil:
		node := LayoutNode{Type: "group"}
		if sizes != nil || children != nil {
			p.fail(n, path, "a group has panels, not sizes or children")
		}
		kinds := p.list(panels, path+".panels", 40)
		if len(kinds) == 0 {
			p.fail(panels, path+".panels", "a group needs at least one panel")
		}
		if len(kinds) > maxGroupPanels {
			p.fail(panels, path+".panels", "a group holds at most %d panels", maxGroupPanels)
		}
		for _, kind := range kinds {
			if !registry.ValidPanel(kind) {
				p.fail(panels, path+".panels", "there is no panel %q%s (panels: %s)", kind, didYouMean(kind, registry.LayoutPanels), strings.Join(registry.LayoutPanels, ", "))
			}
		}
		node.Kinds = kinds
		if minimized != nil {
			node.Minimized = p.boolean(minimized, path+".minimized")
		}
		return node
	case split != nil:
		node := LayoutNode{Type: "split"}
		if minimized != nil {
			p.fail(minimized, path+".minimized", "only a group can be minimized")
		}
		node.Direction = p.str(split, path+".split", 10)
		if node.Direction != "" {
			p.oneOf(split, path+".split", node.Direction, []string{"row", "column"})
		}
		if children == nil || children.Kind != yaml.SequenceNode || len(children.Content) < 2 {
			p.fail(n, path+".children", "a split needs children: a list of at least two parts")
			return node
		}
		if len(children.Content) > maxLayoutChildren {
			p.fail(children, path+".children", "a split has at most %d parts", maxLayoutChildren)
			return node
		}
		for i, child := range children.Content {
			node.Children = append(node.Children, p.layoutNode(child, fmt.Sprintf("%s.children[%d]", path, i), depth+1))
		}
		node.Sizes = p.sizes(sizes, path+".sizes", len(children.Content))
		return node
	}
	p.fail(n, path, "a part needs panels (a group) or split (a split)")
	return LayoutNode{}
}

// sizes reads the shares of a split and normalizes them to sum to 1.
func (p *parser) sizes(n *yaml.Node, path string, count int) []float64 {
	out := make([]float64, count)
	if n == nil {
		for i := range out {
			out[i] = 1 / float64(count)
		}
		return out
	}
	if n.Kind != yaml.SequenceNode || len(n.Content) != count {
		p.fail(n, path, "sizes has one share per child (%d)", count)
		return out
	}
	total := 0.0
	for i, item := range n.Content {
		v, err := strconv.ParseFloat(strings.TrimSuffix(item.Value, "%"), 64)
		if item.Kind != yaml.ScalarNode || err != nil || v <= 0 || math.IsInf(v, 0) || math.IsNaN(v) {
			p.fail(item, path, "%q is not a positive share such as 30 or 0.3", item.Value)
			return out
		}
		out[i], total = v, total+v
	}
	for i := range out {
		out[i] = math.Round(out[i]/total*10000) / 10000
	}
	return out
}
