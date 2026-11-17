package power

import (
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"gopkg.in/yaml.v3"
)

// Config files: keys.yaml, settings.yaml and layouts/*.yaml beside the
// actions file (POWER.md, "Config files"). One loader serves them and the
// actions file: polled every second, a version that does not load leaves the
// last good one in force (marked stale), problems are reported by line. The
// app reads them through GetConfigFiles and writes them through
// UpdateConfigFile, which changes values in place and keeps the comments.

// Config file kinds.
const (
	KindKeys     = "keys"
	KindSettings = "settings"
	KindLayout   = "layout"
	KindActions  = "actions"
)

// loaded is a file as last read.
type loaded[T any] struct {
	Path   string
	Exists bool
	// Err: the file as a whole does not load. Value is then the last version
	// that did (Stale), or the zero value.
	Err      string
	Stale    bool
	Issues   []Issue
	Value    T
	Good     bool
	Revision uint64
	LoadedAt time.Time
}

// loader keeps one file loaded. It is not safe for concurrent use: its owner locks.
type loader[T any] struct {
	path   string
	parse  func([]byte) (T, []Issue, error)
	digest [32]byte
	read   bool
	good   *T
	state  loaded[T]
}

func newLoader[T any](path string, parse func([]byte) (T, []Issue, error)) *loader[T] {
	return &loader[T]{path: path, parse: parse}
}

// reload reads the file again and reports whether anything changed.
func (l *loader[T]) reload() bool {
	data, err := readBounded(l.path)
	digest := sha256.Sum256(data)
	if err != nil {
		digest = sha256.Sum256([]byte("\x00error\x00" + err.Error()))
	}
	if l.read && digest == l.digest {
		return false
	}
	l.read, l.digest = true, digest
	next := loaded[T]{Path: l.path, Exists: err == nil || !errors.Is(err, fs.ErrNotExist), Revision: l.state.Revision + 1, LoadedAt: time.Now()}
	switch {
	case errors.Is(err, fs.ErrNotExist):
		l.good = nil
	case err != nil:
		next.Err = "the file cannot be read: " + err.Error()
	default:
		value, issues, parseErr := l.parse(data)
		next.Issues = issues
		if parseErr != nil {
			next.Err = parseErr.Error()
		} else {
			next.Value, next.Good = value, true
			l.good = &value
		}
	}
	if next.Err != "" && l.good != nil {
		next.Value, next.Stale, next.Good = *l.good, true, true
	}
	l.state = next
	return true
}

// ConfigDoc is one config file as last loaded.
type ConfigDoc struct {
	// Name is keys, settings or layouts/<id>.
	Name, Kind, Path string
	Exists           bool
	Err              string
	Stale            bool
	Issues           []Issue
	// Value is what it declares (Keys, Settings or Layout), nil when nothing loaded.
	Value      any
	Revision   uint64
	LoadedAt   time.Time
	SchemaPath string
}

// ConfigSnapshot is every config file.
type ConfigSnapshot struct {
	Dir      string
	Docs     []ConfigDoc
	Revision uint64
}

// Doc returns the file of this name.
func (s ConfigSnapshot) Doc(name string) (ConfigDoc, bool) {
	i := slices.IndexFunc(s.Docs, func(d ConfigDoc) bool { return d.Name == name })
	if i < 0 {
		return ConfigDoc{}, false
	}
	return s.Docs[i], true
}

// Config keeps the config files of one folder loaded.
type Config struct {
	dir      string
	mu       sync.Mutex
	keys     *loader[Keys]
	settings *loader[Settings]
	layouts  map[string]*loader[Layout]
	snap     ConfigSnapshot
	read     bool
}

// NewConfig loads the config files in dir (which need not exist).
func NewConfig(dir string) *Config {
	c := &Config{dir: dir, layouts: map[string]*loader[Layout]{},
		keys:     newLoader(filepath.Join(dir, "keys.yaml"), ParseKeys),
		settings: newLoader(filepath.Join(dir, "settings.yaml"), ParseSettings)}
	c.Reload()
	return c
}

// Dir is the folder.
func (c *Config) Dir() string { return c.dir }

// LayoutsDir is where layout files live.
func (c *Config) LayoutsDir() string { return filepath.Join(c.dir, "layouts") }

// Snapshot returns the files as last loaded.
func (c *Config) Snapshot() ConfigSnapshot {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.snap
}

// Reload reads every file again and reports whether anything changed.
func (c *Config) Reload() (ConfigSnapshot, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	changed := c.keys.reload()
	changed = c.settings.reload() || changed
	present := map[string]bool{}
	entries, _ := os.ReadDir(c.LayoutsDir())
	for _, e := range entries {
		id, ok := strings.CutSuffix(e.Name(), ".yaml")
		if !ok || e.IsDir() || !LayoutIDPattern.MatchString(id) || len(present) >= MaxLayouts {
			continue
		}
		present[id] = true
		if c.layouts[id] == nil {
			c.layouts[id] = newLoader(filepath.Join(c.LayoutsDir(), e.Name()), ParseLayout)
		}
	}
	for id := range c.layouts {
		if !present[id] {
			delete(c.layouts, id)
			changed = true
		}
	}
	for _, id := range sortedIDs(c.layouts) {
		changed = c.layouts[id].reload() || changed
	}
	if !changed && c.read {
		return c.snap, false
	}
	c.read = true
	next := ConfigSnapshot{Dir: c.dir, Revision: c.snap.Revision + 1}
	next.Docs = append(next.Docs, doc("keys", KindKeys, c.keys.state, filepath.Join(c.dir, SchemaFile(KindKeys))))
	next.Docs = append(next.Docs, doc("settings", KindSettings, c.settings.state, filepath.Join(c.dir, SchemaFile(KindSettings))))
	for _, id := range sortedIDs(c.layouts) {
		next.Docs = append(next.Docs, doc("layouts/"+id, KindLayout, c.layouts[id].state, filepath.Join(c.dir, SchemaFile(KindLayout))))
	}
	c.snap = next
	return next, true
}

func doc[T any](name, kind string, l loaded[T], schema string) ConfigDoc {
	d := ConfigDoc{Name: name, Kind: kind, Path: l.Path, Exists: l.Exists, Err: l.Err, Stale: l.Stale, Issues: l.Issues, Revision: l.Revision, LoadedAt: l.LoadedAt, SchemaPath: schema}
	if l.Good {
		d.Value = l.Value
	}
	return d
}

func sortedIDs[T any](m map[string]*loader[T]) []string {
	ids := make([]string, 0, len(m))
	for id := range m {
		ids = append(ids, id)
	}
	slices.Sort(ids)
	return ids
}

// Watch reloads the files every interval until stop is closed.
func (c *Config) Watch(stop <-chan struct{}, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-stop:
			return
		case <-ticker.C:
			c.Reload()
		}
	}
}

// Edit sets the value at a path of keys from the top of a file; an empty
// JSON removes the key.
type Edit struct {
	Path []string
	JSON string
}

// RefusedError is a change that would leave the file not loading, or with
// new errors; nothing was written.
type RefusedError struct {
	Reason string
	Issues []Issue
}

func (e *RefusedError) Error() string {
	if len(e.Issues) == 0 {
		return e.Reason
	}
	var lines []string
	for _, i := range e.Issues {
		lines = append(lines, i.String())
	}
	return e.Reason + ": " + strings.Join(lines, "; ")
}

// Resolve returns a file's kind and path from its name: keys, settings,
// layouts/<id>, or actions (the actions file, given by the caller).
func (c *Config) Resolve(name string) (kind, path string, err error) {
	switch {
	case name == "keys":
		return KindKeys, filepath.Join(c.dir, "keys.yaml"), nil
	case name == "settings":
		return KindSettings, filepath.Join(c.dir, "settings.yaml"), nil
	case strings.HasPrefix(name, "layouts/"):
		id := strings.TrimSuffix(strings.TrimPrefix(name, "layouts/"), ".yaml")
		if !LayoutIDPattern.MatchString(id) {
			return "", "", fmt.Errorf("%q is not a layout name: lowercase letters, digits and dashes", id)
		}
		return KindLayout, filepath.Join(c.LayoutsDir(), id+".yaml"), nil
	}
	return "", "", fmt.Errorf("no config file %q (keys, settings or layouts/<name>)", name)
}

// Check parses content as a file of this kind would be.
func Check(kind string, data []byte) (value any, issues []Issue, err error) {
	switch kind {
	case KindKeys:
		return wrap(ParseKeys(data))
	case KindSettings:
		return wrap(ParseSettings(data))
	case KindLayout:
		return wrap(ParseLayout(data))
	case KindActions:
		return wrap(Parse(data))
	}
	return nil, nil, fmt.Errorf("unknown kind %q", kind)
}

func wrap[T any](v T, issues []Issue, err error) (any, []Issue, error) { return v, issues, err }

// errorsOf are a check's errors, by path and message (lines move with edits).
func errorsOf(issues []Issue) map[string]bool {
	out := map[string]bool{}
	for _, i := range issues {
		if i.Severity == "error" {
			out[i.Path+"\x00"+i.Message] = true
		}
	}
	return out
}

// Update changes one file: edits in place (comments kept), a whole new
// content, or removal (layouts). It writes nothing when the result would not
// load or would have an error the file did not have before.
func (c *Config) Update(name string, edits []Edit, content []byte, remove bool) (ConfigSnapshot, error) {
	kind, path, err := c.Resolve(name)
	if err != nil {
		return c.Snapshot(), err
	}
	switch {
	case remove:
		if kind != KindLayout {
			return c.Snapshot(), errors.New("only a layout file can be removed here")
		}
		if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return c.Snapshot(), err
		}
	case content != nil:
		if err := writeChecked(kind, path, content, nil); err != nil {
			return c.Snapshot(), err
		}
	default:
		if err := EditFile(kind, path, edits); err != nil {
			return c.Snapshot(), err
		}
	}
	if err := WriteSchemas(c.dir); err != nil {
		return c.Snapshot(), err
	}
	snap, _ := c.Reload()
	return snap, nil
}

// EditFile applies edits to a file of this kind, creating it from its
// starter when it does not exist, and writes it only when it still loads.
func EditFile(kind, path string, edits []Edit) error {
	current, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		current, err = []byte(Starter(kind)), nil
	}
	if err != nil {
		return err
	}
	var before map[string]bool
	if _, issues, parseErr := Check(kind, current); parseErr == nil || errors.Is(parseErr, errLayoutInvalid) {
		before = errorsOf(issues)
	}
	var doc yaml.Node
	if err := yaml.Unmarshal(current, &doc); err != nil {
		return &RefusedError{Reason: "the file is not valid YAML now (" + strings.TrimPrefix(err.Error(), "yaml: ") + "); fix it first"}
	}
	if doc.Kind == 0 {
		doc = yaml.Node{Kind: yaml.DocumentNode, Content: []*yaml.Node{{Kind: yaml.MappingNode, Tag: "!!map"}}}
	}
	for _, e := range edits {
		var value *yaml.Node
		if e.JSON != "" {
			if value, err = jsonToNode([]byte(e.JSON)); err != nil {
				return fmt.Errorf("%s: %w", strings.Join(e.Path, "."), err)
			}
		}
		if err := setPath(&doc, e.Path, value); err != nil {
			return err
		}
	}
	var out bytes.Buffer
	enc := yaml.NewEncoder(&out)
	enc.SetIndent(2)
	if err := enc.Encode(&doc); err != nil {
		return err
	}
	_ = enc.Close()
	return writeChecked(kind, path, out.Bytes(), before)
}

// writeChecked writes data when it loads with no error that before (the
// file's errors until now) did not have.
func writeChecked(kind, path string, data []byte, before map[string]bool) error {
	_, issues, parseErr := Check(kind, data)
	if parseErr != nil && !errors.Is(parseErr, errLayoutInvalid) {
		return &RefusedError{Reason: "the result would not load (" + parseErr.Error() + ")"}
	}
	var fresh []Issue
	for _, i := range issues {
		if i.Severity == "error" && !before[i.Path+"\x00"+i.Message] {
			fresh = append(fresh, i)
		}
	}
	if len(fresh) > 0 {
		return &RefusedError{Reason: "the change has problems", Issues: fresh}
	}
	return writeAtomic(path, data)
}

func writeAtomic(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".*")
	if err != nil {
		return err
	}
	_, err = tmp.Write(data)
	err = errors.Join(err, tmp.Chmod(0o600), tmp.Close())
	if err == nil {
		err = os.Rename(tmp.Name(), path)
	}
	if err != nil {
		_ = os.Remove(tmp.Name())
	}
	return err
}

// setPath sets (or, with a nil value, removes) the value at path in doc,
// creating mappings on the way and keeping the comments of what it replaces.
func setPath(doc *yaml.Node, path []string, value *yaml.Node) error {
	if len(path) == 0 {
		return errors.New("an edit needs a path")
	}
	node := doc.Content[0]
	for i, key := range path {
		if node.Kind == yaml.ScalarNode && node.Tag == "!!null" {
			*node = yaml.Node{Kind: yaml.MappingNode, Tag: "!!map", LineComment: node.LineComment, HeadComment: node.HeadComment}
		}
		if node.Kind != yaml.MappingNode {
			return fmt.Errorf("%s is not a mapping in the file", strings.Join(path[:i], "."))
		}
		at := -1
		for j := 0; j+1 < len(node.Content); j += 2 {
			if node.Content[j].Value == key {
				at = j
				break
			}
		}
		last := i == len(path)-1
		switch {
		case last && value == nil:
			if at >= 0 {
				node.Content = append(node.Content[:at], node.Content[at+2:]...)
			}
			return nil
		case last:
			if at >= 0 {
				old := node.Content[at+1]
				value.LineComment, value.FootComment = old.LineComment, old.FootComment
				if old.HeadComment != "" {
					value.HeadComment = old.HeadComment
				}
				node.Content[at+1] = value
			} else {
				node.Style &^= yaml.FlowStyle
				node.Content = append(node.Content, &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Value: key}, value)
			}
			return nil
		case at >= 0:
			node = node.Content[at+1]
		case value == nil:
			return nil
		default:
			child := &yaml.Node{Kind: yaml.MappingNode, Tag: "!!map"}
			node.Style &^= yaml.FlowStyle
			node.Content = append(node.Content, &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Value: key}, child)
			node = child
		}
	}
	return nil
}

// jsonToNode converts JSON to a YAML node, keeping the order of object keys.
// A short list of scalars is written inline ([Mod+k, "] ] p"]).
func jsonToNode(data []byte) (*yaml.Node, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	node, err := decodeNode(dec)
	if err != nil {
		return nil, fmt.Errorf("not JSON: %w", err)
	}
	if _, err := dec.Token(); !errors.Is(err, io.EOF) {
		return nil, errors.New("not JSON: more than one value")
	}
	return node, nil
}

func decodeNode(dec *json.Decoder) (*yaml.Node, error) {
	tok, err := dec.Token()
	if err != nil {
		return nil, err
	}
	switch v := tok.(type) {
	case json.Delim:
		switch v {
		case '{':
			n := &yaml.Node{Kind: yaml.MappingNode, Tag: "!!map"}
			for dec.More() {
				k, err := dec.Token()
				if err != nil {
					return nil, err
				}
				key, _ := k.(string)
				child, err := decodeNode(dec)
				if err != nil {
					return nil, err
				}
				n.Content = append(n.Content, &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Value: key}, child)
			}
			_, err := dec.Token()
			return n, err
		case '[':
			n := &yaml.Node{Kind: yaml.SequenceNode, Tag: "!!seq"}
			flow := true
			for dec.More() {
				child, err := decodeNode(dec)
				if err != nil {
					return nil, err
				}
				flow = flow && child.Kind == yaml.ScalarNode && len(child.Value) <= 24
				n.Content = append(n.Content, child)
			}
			if flow && len(n.Content) <= 6 {
				n.Style = yaml.FlowStyle
			}
			_, err := dec.Token()
			return n, err
		}
	case string:
		return &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Value: v}, nil
	case json.Number:
		tag := "!!int"
		if strings.ContainsAny(v.String(), ".eE") {
			tag = "!!float"
		}
		return &yaml.Node{Kind: yaml.ScalarNode, Tag: tag, Value: v.String()}, nil
	case bool:
		return &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!bool", Value: fmt.Sprint(v)}, nil
	case nil:
		return &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!null", Value: "null"}, nil
	}
	return nil, fmt.Errorf("unexpected %v", tok)
}

// SchemaFile is a kind's JSON Schema file name in the config folder.
func SchemaFile(kind string) string {
	if kind == KindActions {
		return SchemaName
	}
	return kind + ".schema.json"
}

// WriteSchemas writes the config files' JSON Schemas into dir: they are
// ours, so they follow this Runner (and its action registry).
func WriteSchemas(dir string) error {
	for _, kind := range []string{KindKeys, KindSettings, KindLayout} {
		path := filepath.Join(dir, SchemaFile(kind))
		data := []byte(SchemaFor(kind))
		if current, err := os.ReadFile(path); err == nil && bytes.Equal(current, data) {
			continue
		}
		if err := writeAtomic(path, data); err != nil {
			return err
		}
	}
	return nil
}

// Starter is a new file of this kind: the schema header and a commented example.
func Starter(kind string) string {
	switch kind {
	case KindKeys:
		return `# yaml-language-server: $schema=./keys.schema.json
#
# Your Lazybox keyboard shortcuts. Lazybox reloads this file when you save it,
# and Settings → Shortcuts writes to it (keeping your comments).
#
# preset gives every action its keys; keys: changes some of them, by section:
#   global (anywhere), navigation (the sidebar), agent (the message box).
# A key is "Mod+k" (⌘ on a Mac, Ctrl elsewhere), "Ctrl+Shift+p", a sequence
# "] ] i", "a or b", or a list; [] removes the shortcut. Every action and its
# keys: lazybox do --list. A mistake is reported in Settings with its line;
# the rest keeps working.
#
# keys:
#   global:
#     palette.open: Mod+k
#     layout.cycle: [Mod+Shift+l, "] ] L"]
# leader-menu:
#   - {key: f, label: Fix the tests, snippet: Fix the failing tests., mode: send}
# vim: {message-box: true, file-editor: false}
version: 1
preset: vim
`
	case KindSettings:
		return `# yaml-language-server: $schema=./settings.schema.json
#
# Lazybox settings you may keep in a file. Lazybox reloads it when you save,
# and the app writes to it (keeping your comments). Credentials,
# subscriptions, devices and the Runner's own settings are never here.
# What the file leaves out is the default.
#
# notifications:             # Settings → Notifications writes here
#   needs-input: true
#   review: false
` + appearanceStarter() + `version: 1
`
	case KindLayout:
		return `# yaml-language-server: $schema=../layout.schema.json
#
# A whole-window layout: the Layout menu, the palette and ⌘⇧L offer it.
name: My layout
layout:
  split: row
  sizes: [20, 80]
  children:
    - split: column
      sizes: [62, 38]
      children:
        - panels: [collections]
        - panels: [agents]
    - panels: [chat]
`
	}
	return ""
}

// SchemaFor is a config file kind's JSON Schema (2020-12), built from the
// action registry so it names every action and panel.
func SchemaFor(kind string) string {
	var schema map[string]any
	switch kind {
	case KindKeys:
		binding := map[string]any{"anyOf": []any{
			map[string]any{"type": "string", "description": `"Mod+k", "] ] i", or "a or b"`},
			map[string]any{"type": "array", "items": map[string]any{"type": "string"}, "description": "Several bindings; [] for none"},
			map[string]any{"type": "null"},
		}}
		sections := map[string]any{}
		for _, section := range KeySections {
			props := map[string]any{}
			for _, a := range registry.Actions {
				if a.Scope == section && !a.Fixed {
					entry := map[string]any{"$ref": "#/$defs/binding", "description": a.Title + " — default " + a.Keys}
					props[a.ID] = entry
				}
			}
			sections[section] = map[string]any{"type": "object", "additionalProperties": false, "properties": props}
		}
		schema = map[string]any{
			"$schema": "https://json-schema.org/draft/2020-12/schema", "$id": "urn:lazybox:schema:keys:v1", "title": "Lazybox keyboard shortcuts (keys.yaml)", "type": "object", "additionalProperties": false,
			"$defs": map[string]any{"binding": binding},
			"properties": map[string]any{
				"version": map[string]any{"const": 1},
				"preset":  map[string]any{"enum": registry.Presets, "description": "The keys of every action the file leaves out"},
				"keys":    map[string]any{"type": "object", "additionalProperties": false, "properties": sections},
				"leader-menu": map[string]any{"type": "array", "maxItems": 36, "items": map[string]any{"type": "object", "additionalProperties": false, "required": []string{"key", "snippet"},
					"properties": map[string]any{"key": map[string]any{"type": "string", "pattern": `^(?:[a-z0-9]|Shift\+[a-z])$`}, "label": map[string]any{"type": "string", "maxLength": 60}, "snippet": map[string]any{"type": "string"}, "mode": map[string]any{"enum": []string{"send", "insert"}}}}},
				"vim": map[string]any{"type": "object", "additionalProperties": false, "properties": map[string]any{"message-box": map[string]any{"type": "boolean"}, "file-editor": map[string]any{"type": "boolean"}}},
			},
		}
	case KindSettings:
		props := map[string]any{}
		for _, name := range notificationOrder {
			props[name] = map[string]any{"type": "boolean"}
		}
		schema = map[string]any{
			"$schema": "https://json-schema.org/draft/2020-12/schema", "$id": "urn:lazybox:schema:settings:v1", "title": "Lazybox settings (settings.yaml)", "type": "object", "additionalProperties": false,
			"properties": map[string]any{
				"version":       map[string]any{"const": 1},
				"notifications": map[string]any{"type": "object", "additionalProperties": false, "properties": props},
				"appearance":    appearanceSchema(),
			},
		}
	case KindLayout:
		node := map[string]any{"oneOf": []any{
			map[string]any{"type": "object", "additionalProperties": false, "required": []string{"panels"}, "properties": map[string]any{
				"panels":    map[string]any{"type": "array", "minItems": 1, "maxItems": maxGroupPanels, "items": map[string]any{"enum": registry.LayoutPanels}},
				"minimized": map[string]any{"type": "boolean"},
			}},
			map[string]any{"type": "object", "additionalProperties": false, "required": []string{"split", "children"}, "properties": map[string]any{
				"split":    map[string]any{"enum": []string{"row", "column"}},
				"sizes":    map[string]any{"type": "array", "items": map[string]any{"type": "number", "exclusiveMinimum": 0}},
				"children": map[string]any{"type": "array", "minItems": 2, "maxItems": maxLayoutChildren, "items": map[string]any{"$ref": "#/$defs/node"}},
			}},
		}}
		schema = map[string]any{
			"$schema": "https://json-schema.org/draft/2020-12/schema", "$id": "urn:lazybox:schema:layout:v1", "title": "A Lazybox layout (layouts/*.yaml)", "type": "object", "additionalProperties": false, "required": []string{"name", "layout"},
			"$defs": map[string]any{"node": node},
			"properties": map[string]any{
				"version":     map[string]any{"const": 1},
				"name":        map[string]any{"type": "string", "maxLength": 48},
				"description": map[string]any{"type": "string", "maxLength": 120},
				"layout":      map[string]any{"$ref": "#/$defs/node"},
			},
		}
	case KindActions:
		return Schema
	default:
		return ""
	}
	data, _ := json.MarshalIndent(schema, "", "  ")
	return string(data) + "\n"
}
