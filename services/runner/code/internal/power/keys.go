package power

import (
	"errors"
	"fmt"
	"io"
	"regexp"
	"slices"
	"strings"

	"gopkg.in/yaml.v3"
)

// keys.yaml: the person's keyboard shortcuts (POWER.md, "Config files").
//
//	version: 1
//	preset: vim                 # vim, tmux or arrows: the keys of every action left out
//	keys:
//	  global:                   # sections are the actions' scopes: global, navigation, agent
//	    palette.open: Mod+k     # one binding, "a or b", or a list; [] for none
//	    layout.cycle: [Mod+Shift+l, "] ] L"]
//	leader-menu:                # your own entries under the ]] leader
//	  - {key: f, label: Fix it, snippet: fix, mode: send}
//	vim: {message-box: false, file-editor: false}
//
// What the file leaves out is the default. Keys are checked as the app checks
// them (actions.ts, keyboard.ts): names, sequences of at most four keys,
// conflicts between actions that can fire from the same place, the reset
// chord no one may take. A binding with a problem is left out (the action
// keeps its preset key) and reported with its line.

// KeySections are the sections of keys:, the registry's scopes.
var KeySections = []string{"global", "navigation", "agent"}

// Keys is what keys.yaml declares. Bindings keep Mod+ as written: the app
// resolves it for its platform.
type Keys struct {
	Preset   string       `json:"preset"`
	Bindings []KeyBinding `json:"bindings"`
	Menu     []MenuEntry  `json:"menu"`
	Vim      VimKeys      `json:"vim"`
}

// KeyBinding is one action's keys; an empty list means no shortcut.
type KeyBinding struct {
	Action  string   `json:"action"`
	Keys    []string `json:"keys"`
	Section string   `json:"section"`
	Line    int      `json:"line"`
}

// MenuEntry is one key under the ]] leader that sends or inserts a snippet.
type MenuEntry struct {
	Key     string `json:"key"`
	Label   string `json:"label"`
	Snippet string `json:"snippet"`
	Mode    string `json:"mode"`
	Line    int    `json:"line"`
}

// VimKeys switches Vim editing on where you type.
type VimKeys struct {
	MessageBox bool `json:"messageBox"`
	FileEditor bool `json:"fileEditor"`
}

var modifierOrder = []string{"Ctrl", "Meta", "Alt", "Shift"}
var modifierAliases = map[string]string{"Cmd": "Meta", "Command": "Meta", "Control": "Ctrl", "Option": "Alt", "Ctrl": "Ctrl", "Meta": "Meta", "Alt": "Alt", "Shift": "Shift", "Mod": "Mod"}
var keyNamePattern = regexp.MustCompile(`^(?:(?:(?:Ctrl|Meta|Alt|Shift)\+)*(?:[^\s+]|\+|Enter|Escape|Tab|Space|ArrowDown|ArrowUp|ArrowLeft|ArrowRight|Backspace|Delete|Home|End|PageUp|PageDown|F[1-9][0-9]?)|AltRight|MetaRight)$`)

// heldKeyPattern is a key held down to talk: a chord with Alt, Ctrl or Meta,
// a function key, or a right-hand modifier alone (actions.ts, loneModifiers).
var heldKeyPattern = regexp.MustCompile(`(?:^|\+)(?:Ctrl|Meta|Alt)\+|^F\d+$|^(?:AltRight|MetaRight)$`)

// loneModifierPattern is a right-hand modifier alone, which only a held key may be.
var loneModifierPattern = regexp.MustCompile(`^(?:AltRight|MetaRight)$`)
var orPattern = regexp.MustCompile(`\s+or\s+`)
var menuKeyPattern = regexp.MustCompile(`^(?:[a-z0-9]|Shift\+[a-z])$`)

// splitBindings splits "a or b" into its alternatives, as actions.ts does.
func splitBindings(value string) []string {
	var out []string
	for _, b := range orPattern.Split(value, -1) {
		if b = strings.Join(strings.Fields(b), " "); b != "" {
			out = append(out, b)
		}
	}
	return out
}

// canonicalKey orders a chord's modifiers and lowercases a lone letter; it
// reports a modifier it does not know.
func canonicalKey(key string) (string, error) {
	parts := strings.Split(key, "+")
	name := parts[len(parts)-1]
	parts = parts[:len(parts)-1]
	if name == "" && len(parts) > 0 {
		parts, name = parts[:len(parts)-1], "+"
	}
	mods := map[string]bool{}
	for _, p := range parts {
		m, ok := modifierAliases[p]
		if !ok {
			return "", fmt.Errorf("%q is not a modifier (use Mod, Ctrl, Meta, Alt or Shift)", p)
		}
		if m == "Mod" {
			m = "Ctrl"
			if platform() == "mac" {
				m = "Meta"
			}
		}
		mods[m] = true
	}
	if len([]rune(name)) == 1 {
		name = strings.ToLower(name)
	}
	var b strings.Builder
	for _, m := range modifierOrder {
		if mods[m] {
			b.WriteString(m + "+")
		}
	}
	b.WriteString(name)
	return b.String(), nil
}

// canonicalBinding is a sequence of canonical keys.
func canonicalBinding(binding string) (string, error) {
	var keys []string
	for _, k := range strings.Fields(binding) {
		c, err := canonicalKey(k)
		if err != nil {
			return "", err
		}
		keys = append(keys, c)
	}
	return strings.Join(keys, " "), nil
}

// checkBinding reports what is wrong with one binding of an action, or "".
func checkBinding(a RegistryAction, binding string) string {
	canonical, err := canonicalBinding(binding)
	if err != nil {
		return err.Error()
	}
	keys := strings.Fields(canonical)
	if len(keys) > 4 {
		return fmt.Sprintf("%q has %d keys; a shortcut has at most four", binding, len(keys))
	}
	for _, k := range keys {
		if !keyNamePattern.MatchString(k) {
			return fmt.Sprintf("%q is not a key: use a character or a name such as Enter, Escape, Tab, Space, ArrowDown or F5, with Mod+, Ctrl+, Meta+, Alt+ or Shift+", k)
		}
	}
	if a.Hold && (len(keys) > 1 || !heldKeyPattern.MatchString(keys[0])) {
		return fmt.Sprintf("%s is held down: it needs one key held with Alt, Ctrl or Mod, a function key, or AltRight or MetaRight alone, such as Alt+Space", a.Title)
	}
	if !a.Hold {
		for _, k := range keys {
			if loneModifierPattern.MatchString(k) {
				return fmt.Sprintf("%q is a modifier alone: only a key held to talk (voice.dictate, voice.command) can be AltRight or MetaRight", k)
			}
		}
	}
	return ""
}

// overlaps: two actions can fire from the same place.
func overlaps(a, b string) bool { return a == b || a == "global" || b == "global" }

// ParseKeys reads keys.yaml. An error means the file as a whole cannot be
// used; otherwise every binding or entry with a problem is left out and
// reported, and the rest load.
func ParseKeys(data []byte) (Keys, []Issue, error) {
	root, err := parseRoot(data, "the file must be a mapping with version, preset, keys, leader-menu and vim")
	if err != nil || root == nil {
		return Keys{Bindings: []KeyBinding{}, Menu: []MenuEntry{}}, nil, err
	}
	p := &parser{}
	out := Keys{Preset: "vim", Bindings: []KeyBinding{}, Menu: []MenuEntry{}}
	var menuNode *yaml.Node
	for key, value := range pairs(root) {
		switch key.Value {
		case "version":
			if value.Value != "1" {
				p.fail(value, "version", "only version 1 exists")
			}
		case "preset":
			if v := p.str(value, "preset", 20); v != "" && p.oneOf(value, "preset", v, registry.Presets) {
				out.Preset = v
			}
		case "keys":
			out.Bindings = p.keySections(value)
		case "leader-menu":
			menuNode = value
		case "vim":
			out.Vim = p.vim(value)
		default:
			p.warn(key, key.Value, "unknown section %q is ignored (expected version, preset, keys, leader-menu, vim)%s", key.Value, didYouMean(key.Value, []string{"version", "preset", "keys", "leader-menu", "vim"}))
		}
	}
	out.Bindings = p.resolveConflicts(out.Preset, out.Bindings)
	if menuNode != nil && menuNode.Tag != "!!null" {
		out.Menu = p.leaderMenu(menuNode, out.Preset, out.Bindings)
	}
	return out, p.issues, nil
}

func (p *parser) keySections(n *yaml.Node) []KeyBinding {
	out := []KeyBinding{}
	if n.Tag == "!!null" {
		return out
	}
	if n.Kind != yaml.MappingNode {
		p.fail(n, "keys", "keys is a mapping of sections (global, navigation, agent) to actions and their keys")
		return out
	}
	seen := map[string]int{}
	for section, actions := range pairs(n) {
		at := "keys." + section.Value
		if !slices.Contains(KeySections, section.Value) {
			p.fail(section, at, "unknown section %q (expected %s)%s", section.Value, strings.Join(KeySections, ", "), didYouMean(section.Value, KeySections))
			continue
		}
		if actions.Tag == "!!null" {
			continue
		}
		if actions.Kind != yaml.MappingNode {
			p.fail(actions, at, "a section maps action ids to their keys, such as palette.open: Mod+k")
			continue
		}
		for id, value := range pairs(actions) {
			path := at + "." + id.Value
			a, ok := registry.Action(id.Value)
			if !ok {
				p.fail(id, path, "there is no action %q%s (lazybox do --list lists every action)", id.Value, didYouMean(id.Value, registry.ActionIDs()))
				continue
			}
			if line, dup := seen[a.ID]; dup {
				p.fail(id, path, "%s is already set on line %d", a.ID, line)
				continue
			}
			seen[a.ID] = id.Line
			if a.Scope != section.Value {
				p.warn(id, path, "%s belongs under %s: (it still applies here)", a.ID, a.Scope)
			}
			if a.Fixed {
				p.fail(id, path, "%s cannot be changed: it is the way back from any keymap", a.Title)
				continue
			}
			var values []string
			switch {
			case value.Tag == "!!null":
			case value.Kind == yaml.ScalarNode:
				values = splitBindings(value.Value)
			case value.Kind == yaml.SequenceNode:
				for _, item := range value.Content {
					if item.Kind != yaml.ScalarNode {
						p.fail(item, path, "each binding is text, such as Mod+k or \"] ] p\"")
						continue
					}
					values = append(values, splitBindings(item.Value)...)
				}
			default:
				p.fail(value, path, "keys are text (\"Mod+k or ] ] p\") or a list of them; [] for none")
				continue
			}
			bad := false
			for _, b := range values {
				if problem := checkBinding(a, b); problem != "" {
					p.fail(value, path, "%s", problem)
					bad = true
					break
				}
			}
			if bad {
				continue
			}
			if len(values) == 0 && a.Required {
				p.fail(value, path, "%s needs a shortcut", a.Title)
				continue
			}
			if values == nil {
				values = []string{}
			}
			out = append(out, KeyBinding{Action: a.ID, Keys: values, Section: section.Value, Line: id.Line})
		}
	}
	return out
}

// resolveConflicts applies the bindings over the preset and leaves out each
// binding that takes a key another action can fire with from the same place.
func (p *parser) resolveConflicts(preset string, bindings []KeyBinding) []KeyBinding {
	keymap := registry.Keymap(preset)
	fromFile := map[string]*KeyBinding{}
	for i := range bindings {
		b := &bindings[i]
		var canonical []string
		for _, k := range b.Keys {
			c, _ := canonicalBinding(k)
			canonical = append(canonical, c)
		}
		keymap[b.Action] = strings.Join(canonical, " or ")
		fromFile[b.Action] = b
	}
	dropped := map[string]bool{}
	for range len(bindings) + 1 {
		type owner struct{ binding, action string }
		var owners []owner
		var loser, winner, key string
		for _, a := range registry.Actions {
			for _, binding := range splitBindings(keymap[a.ID]) {
				if i := slices.IndexFunc(owners, func(o owner) bool {
					other, _ := registry.Action(o.action)
					return o.binding == binding && o.action != a.ID && overlaps(a.Scope, other.Scope)
				}); i >= 0 {
					x, y := fromFile[a.ID], fromFile[owners[i].action]
					switch {
					case x != nil && (y == nil || x.Line >= y.Line):
						loser, winner = a.ID, owners[i].action
					case y != nil:
						loser, winner = owners[i].action, a.ID
					}
					key = binding
					break
				}
				owners = append(owners, owner{binding, a.ID})
			}
			if loser != "" {
				break
			}
		}
		if loser == "" {
			break
		}
		lost := fromFile[loser]
		other, _ := registry.Action(winner)
		where := "by default"
		if w := fromFile[winner]; w != nil {
			where = fmt.Sprintf("on line %d", w.Line)
		}
		p.issues = append(p.issues, Issue{Path: "keys." + lost.Section + "." + loser, Line: lost.Line, Severity: "error",
			Message: fmt.Sprintf("%q is already %s's key (%s); this binding is left out", key, other.Title, where)})
		dropped[loser] = true
		delete(fromFile, loser)
		keymap[loser] = registry.Keymap(preset)[loser]
	}
	out := []KeyBinding{}
	for _, b := range bindings {
		if !dropped[b.Action] {
			out = append(out, b)
		}
	}
	return out
}

func (p *parser) leaderMenu(n *yaml.Node, preset string, bindings []KeyBinding) []MenuEntry {
	out := []MenuEntry{}
	if n.Kind != yaml.SequenceNode {
		p.fail(n, "leader-menu", "leader-menu is a list of {key, label, snippet, mode}")
		return out
	}
	keymap := registry.Keymap(preset)
	for _, b := range bindings {
		var canonical []string
		for _, k := range b.Keys {
			c, _ := canonicalBinding(k)
			canonical = append(canonical, c)
		}
		keymap[b.Action] = strings.Join(canonical, " or ")
	}
	leader := "] ]"
	if bs := splitBindings(keymap["navigation.exit"]); len(bs) > 0 {
		leader = bs[0]
	}
	taken := map[string]string{}
	for _, a := range registry.Actions {
		for _, b := range splitBindings(keymap[a.ID]) {
			if rest, ok := strings.CutPrefix(b, leader+" "); ok {
				taken[rest] = a.Title
			}
		}
	}
	for i, item := range n.Content {
		path := fmt.Sprintf("leader-menu[%d]", i)
		if i >= 36 {
			p.fail(item, path, "the menu holds at most 36 entries")
			break
		}
		p.failed = false
		e := MenuEntry{Mode: "send", Line: item.Line}
		if item.Kind != yaml.MappingNode {
			p.fail(item, path, "an entry is {key, label, snippet, mode}")
			continue
		}
		for key, value := range pairs(item) {
			at := path + "." + key.Value
			switch key.Value {
			case "key":
				e.Key = p.str(value, at, 7)
				if e.Key != "" && !menuKeyPattern.MatchString(e.Key) {
					p.fail(value, at, "%q must be one lowercase letter or digit (or Shift+letter); it follows the leader", e.Key)
				}
			case "label":
				e.Label = p.str(value, at, 60)
			case "snippet":
				e.Snippet = p.str(value, at, 32<<10)
			case "mode":
				if e.Mode = p.str(value, at, 10); e.Mode != "" {
					p.oneOf(value, at, e.Mode, []string{"send", "insert"})
				}
			default:
				p.fail(key, at, "unknown field %q (expected key, label, snippet, mode)", key.Value)
			}
		}
		if e.Key == "" {
			p.fail(item, path+".key", "an entry needs a key")
		}
		if strings.TrimSpace(e.Snippet) == "" {
			p.fail(item, path+".snippet", "an entry needs a snippet")
		}
		if by, ok := taken[e.Key]; ok && e.Key != "" && !p.failed {
			p.fail(item, path+".key", "%s %s is already %s", leader, e.Key, by)
		}
		if p.failed {
			continue
		}
		taken[e.Key] = e.Label
		out = append(out, e)
	}
	return out
}

func (p *parser) vim(n *yaml.Node) VimKeys {
	out := VimKeys{}
	if n.Tag == "!!null" {
		return out
	}
	if n.Kind != yaml.MappingNode {
		p.fail(n, "vim", "vim is a mapping: message-box, file-editor")
		return out
	}
	for key, value := range pairs(n) {
		at := "vim." + key.Value
		switch key.Value {
		case "message-box":
			out.MessageBox = p.boolean(value, at)
		case "file-editor":
			out.FileEditor = p.boolean(value, at)
		default:
			p.fail(key, at, "unknown field %q (expected message-box, file-editor)", key.Value)
		}
	}
	return out
}

// parseRoot decodes one YAML document that must be a mapping; nil for an
// empty file.
func parseRoot(data []byte, shape string) (*yaml.Node, error) {
	if len(data) > MaxFileBytes {
		return nil, fmt.Errorf("the file is larger than %d KiB", MaxFileBytes>>10)
	}
	var doc yaml.Node
	decoder := yaml.NewDecoder(strings.NewReader(string(data)))
	if err := decoder.Decode(&doc); err != nil {
		if errors.Is(err, io.EOF) {
			return nil, nil
		}
		return nil, fmt.Errorf("not valid YAML: %s", strings.TrimPrefix(err.Error(), "yaml: "))
	}
	var extra yaml.Node
	if err := decoder.Decode(&extra); err == nil {
		return nil, errors.New("the file holds more than one YAML document; keep one")
	}
	root := &doc
	if root.Kind == yaml.DocumentNode && len(root.Content) == 1 {
		root = root.Content[0]
	}
	if root.Kind == yaml.ScalarNode && root.Tag == "!!null" {
		return nil, nil
	}
	if root.Kind != yaml.MappingNode {
		return nil, errors.New(shape)
	}
	return root, nil
}
