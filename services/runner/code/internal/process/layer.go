package process

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io/fs"
	"reflect"
	"slices"
	"time"

	"gopkg.in/yaml.v3"
)

// LayerKind is where a process file comes from.
type LayerKind string

const (
	LayerPreset     LayerKind = "preset"
	LayerUser       LayerKind = "user"
	LayerRepository LayerKind = "repository"
)

// RepositoryFile is where a repository keeps its process, committed.
const RepositoryFile = ".lazybox/process.yaml"

// MaxFileBytes bounds a process file; a repository's file is untrusted input.
const MaxFileBytes = 256 << 10

// File is one parsed layer.
type File struct {
	Kind   LayerKind
	Source string // file path, or the preset name
	// Digest is sha256 of the bytes as read: what a person approves before a
	// repository file's first use.
	Digest  string
	Process *Process // nil when the file has errors
	Issues  Issues
	node    *yaml.Node
}

func (f *File) layer() string {
	if f.Kind == LayerPreset {
		return "preset:" + f.Source
	}
	return string(f.Kind)
}

// source is where an issue in this file is shown: its path, or which preset.
func (f *File) source() string {
	if f.Kind == LayerPreset {
		return "preset " + f.Source
	}
	return f.Source
}

// Parse reads one layer. Structural problems (unknown keys, wrong types,
// values off an enum) and what this kind of layer may not say are reported
// in Issues with their line; Process is nil when there is any error.
func Parse(kind LayerKind, source string, data []byte) *File {
	sum := sha256.Sum256(data)
	f := &File{Kind: kind, Source: source, Digest: "sha256:" + hex.EncodeToString(sum[:])}
	fail := func(n *yaml.Node, path Path, format string, args ...any) {
		i := Issue{Severity: Error, Path: path.String(), Message: fmt.Sprintf(format, args...)}
		if n != nil {
			i.Line, i.Column = n.Line, n.Column
		}
		f.Issues = append(f.Issues, i)
	}
	defer func() {
		for i := range f.Issues {
			f.Issues[i].Layer, f.Issues[i].Source = f.layer(), f.source()
		}
	}()
	if len(data) > MaxFileBytes {
		fail(nil, nil, "file is %d bytes; the limit is %d", len(data), MaxFileBytes)
		return f
	}
	var doc yaml.Node
	if err := yaml.Unmarshal(data, &doc); err != nil {
		fail(nil, nil, "%v", err)
		return f
	}
	if doc.Kind != yaml.DocumentNode || len(doc.Content) == 0 {
		fail(nil, nil, "empty file; a process file starts with version: 1")
		return f
	}
	f.node = &doc
	root := doc.Content[0]
	f.Issues = append(f.Issues, checkNode(root)...)
	if len(f.Issues.Errors()) > 0 {
		return f
	}
	p := &Process{}
	if err := root.Decode(p); err != nil {
		fail(root, nil, "%v", err)
		return f
	}
	if p.Ceiling != nil && kind != LayerUser {
		n, _ := lookup(&doc, Path{"ceiling"})
		fail(n, Path{"ceiling"}, "only your user process file may set a ceiling; a %s cannot raise or lower its own limits", kind)
	}
	if p.Extends != "" {
		n, _ := lookup(&doc, Path{"extends"})
		switch {
		case kind == LayerPreset:
			fail(n, Path{"extends"}, "a preset cannot extend another")
		case !slices.Contains(Presets(), p.Extends):
			fail(n, Path{"extends"}, "unknown preset %q; the presets are %v", p.Extends, Presets())
		}
	}
	if len(f.Issues.Errors()) == 0 {
		f.Process = p
	}
	return f
}

// DefaultCeiling applies when the user file sets none: reviewed-team's
// limits. A repository can choose solo-pair or reviewed-team but not factory
// until the person raises their own ceiling.
func DefaultCeiling() Ceiling {
	return Ceiling{
		Autonomy:   4,
		Permission: "yolo",
		Providers:  []string{"claude", "codex"},
		Budgets: &Budgets{
			Mission: &Budget{USD: 60, Wall: Duration(10 * time.Hour)},
			Task:    &Budget{USD: 12, Turns: 120, Retries: 2},
		},
	}
}

// Options says what Resolve resolves for.
type Options struct {
	// Preset is used when no layer extends one; empty selects
	// DefaultPreset(Tasks).
	Preset string
	// Tasks is the number of tasks in the mission (0 when there is none yet).
	Tasks int
	// Isolation is what this Runner provides for builders: "none" (or empty)
	// or "container".
	Isolation string
	// Files is the repository checkout, to warn about missing instruction
	// files; nil skips that check.
	Files fs.FS
}

// LayerInfo describes one layer that went into an effective process.
type LayerInfo struct {
	Kind   LayerKind `json:"kind"`
	Source string    `json:"source"`
	Digest string    `json:"digest"`
}

// Effective is a resolved process: a preset, overlaid by the user's file,
// overlaid by the repository's, checked against the user's ceiling.
type Effective struct {
	// Process is nil when a layer could not be parsed. Ceiling is folded in:
	// unset budgets take the ceiling's values; Process.Ceiling is nil and
	// Process.Extends names the preset, so Process.YAML() resolves to itself.
	Process *Process    `json:"process,omitempty"`
	Ceiling Ceiling     `json:"ceiling"`
	Preset  string      `json:"preset"`
	Layers  []LayerInfo `json:"layers"`
	// Digest is sha256 of Process's canonical JSON: what a mission pins.
	Digest string `json:"digest,omitempty"`
	Issues Issues `json:"issues,omitempty"`
}

// Valid reports whether the process has no error.
func (e *Effective) Valid() bool { return e.Process != nil && len(e.Issues.Errors()) == 0 }

// Resolve layers a preset, the user's file and a repository's file (either may
// be nil) and validates the result. It never fails: problems are Issues.
func Resolve(user, repository *File, opts Options) *Effective {
	e := &Effective{Ceiling: DefaultCeiling()}
	defer func() { e.Issues.sort() }()
	var layers []*File
	for _, f := range []*File{user, repository} {
		if f != nil {
			layers = append(layers, f)
			e.Issues = append(e.Issues, f.Issues...)
		}
	}
	e.Preset = opts.Preset
	if e.Preset == "" {
		e.Preset = DefaultPreset(opts.Tasks)
	}
	for _, f := range layers {
		if f.Process != nil && f.Process.Extends != "" {
			e.Preset = f.Process.Extends
		}
	}
	preset, err := Preset(e.Preset)
	if err != nil {
		e.Issues = append(e.Issues, Issue{Severity: Error, Path: "extends", Message: err.Error()})
		return e
	}
	layers = append([]*File{preset}, layers...)
	for _, f := range layers {
		e.Layers = append(e.Layers, LayerInfo{Kind: f.Kind, Source: f.Source, Digest: f.Digest})
		if f.Process == nil {
			return e // the file's own issues say why
		}
	}

	merged := deepCopy(preset.Process)
	for _, f := range layers[1:] {
		overlayer := f.Process
		if f.Kind == LayerUser {
			overlayer = e.userDefaults(f, merged)
			if f.Process.Ceiling != nil {
				overlay(reflect.ValueOf(&e.Ceiling).Elem(), reflect.ValueOf(*f.Process.Ceiling))
			}
		}
		overlay(reflect.ValueOf(merged).Elem(), reflect.ValueOf(overlayer).Elem())
	}
	// Extends names the preset it was resolved from, so the effective
	// process written out resolves to itself again.
	merged.Ceiling, merged.Extends, merged.Schema = nil, e.Preset, ""
	merged.Budgets = fillBudgets(merged.Budgets, e.Ceiling.Budgets)
	for _, name := range merged.Roles.Keys {
		if r := merged.Roles.Values[name]; r.Permission == "" {
			r.Permission = "read_only"
		}
	}
	e.Process = merged

	issues := validate(merged, e.Ceiling, opts)
	for i := range issues {
		attribute(&issues[i], layers)
	}
	e.Issues = append(e.Issues, issues...)
	canonical, err := json.Marshal(merged)
	if err != nil {
		e.Issues = append(e.Issues, Issue{Severity: Error, Message: err.Error()})
		return e
	}
	sum := sha256.Sum256(canonical)
	e.Digest = "sha256:" + hex.EncodeToString(sum[:])
	return e
}

// userDefaults is the user's file as an overlay. Its role entries are
// defaults for roles the preset has; one naming a role the preset lacks is
// not applied (a half-written role would fail validation for no reason the
// person chose), and says so.
func (e *Effective) userDefaults(f *File, base *Process) *Process {
	p := deepCopy(f.Process)
	for _, name := range slices.Clone(p.Roles.Keys) {
		if _, ok := base.Roles.Get(name); ok {
			continue
		}
		p.Roles.Delete(name)
		i := Issue{Severity: Warning, Path: Path{"roles", name}.String(), Message: fmt.Sprintf("preset %s has no %s role; your defaults for it are not applied", e.Preset, name), Layer: f.layer(), Source: f.source()}
		if n, _ := lookup(f.node, Path{"roles", name}); n != nil {
			i.Line, i.Column = n.Line, n.Column
		}
		e.Issues = append(e.Issues, i)
	}
	return p
}

// fillBudgets gives every budget the process leaves unset the ceiling's value,
// so "unlimited" never means "above the ceiling".
func fillBudgets(b, ceiling *Budgets) *Budgets {
	if ceiling == nil {
		return b
	}
	out := deepCopy(ceiling)
	out.OnExhausted = ""
	if b != nil {
		overlay(reflect.ValueOf(out).Elem(), reflect.ValueOf(b).Elem())
	}
	return out
}

// attribute finds the layer that wrote an issue's path: the highest layer
// that has the whole path, else the one that has most of it.
func attribute(i *Issue, layers []*File) {
	if i.Layer != "" || i.path == nil {
		return
	}
	best, bestDepth := -1, -1
	var bestNode *yaml.Node
	for idx := len(layers) - 1; idx >= 0; idx-- {
		n, depth := lookup(layers[idx].node, i.path)
		if depth > bestDepth {
			best, bestDepth, bestNode = idx, depth, n
		}
		if depth == len(i.path) {
			break
		}
	}
	if best < 0 || bestDepth == 0 {
		return
	}
	i.Layer, i.Source = layers[best].layer(), layers[best].source()
	i.Line, i.Column = bestNode.Line, bestNode.Column
}

// overlay writes every value src sets onto dst: scalars when non-zero,
// pointers and structs field by field, slices replace, maps and Map merge by
// key (a nil Map value deletes), fields tagged merge:"replace" replace.
func overlay(dst, src reflect.Value) {
	switch src.Kind() {
	case reflect.Pointer:
		if src.IsNil() {
			return
		}
		if dst.IsNil() {
			dst.Set(copyValue(src))
			return
		}
		overlay(dst.Elem(), src.Elem())
	case reflect.Struct:
		if m, ok := dst.Addr().Interface().(mapOverlayer); ok {
			m.overlayFrom(src)
			return
		}
		for i := range src.NumField() {
			field := src.Type().Field(i)
			if !field.IsExported() {
				continue
			}
			if field.Tag.Get("merge") == "replace" {
				if !src.Field(i).IsZero() {
					dst.Field(i).Set(copyValue(src.Field(i)))
				}
				continue
			}
			overlay(dst.Field(i), src.Field(i))
		}
	case reflect.Slice:
		if !src.IsNil() {
			dst.Set(copyValue(src))
		}
	case reflect.Map:
		if src.Len() == 0 {
			return
		}
		if dst.IsNil() {
			dst.Set(reflect.MakeMap(src.Type()))
		}
		for _, k := range src.MapKeys() {
			dst.SetMapIndex(k, copyValue(src.MapIndex(k)))
		}
	default:
		if !src.IsZero() {
			dst.Set(src)
		}
	}
}

type mapOverlayer interface{ overlayFrom(src reflect.Value) }

func (m *Map[T]) overlayFrom(src reflect.Value) {
	from := src.Interface().(Map[T])
	for _, k := range from.Keys {
		v := from.Values[k]
		value := reflect.ValueOf(&v).Elem()
		if value.Kind() == reflect.Pointer && value.IsNil() {
			m.Delete(k)
			continue
		}
		if existing, ok := m.Values[k]; ok {
			target := reflect.ValueOf(&existing).Elem()
			overlay(target, value)
			m.Values[k] = existing
			continue
		}
		m.Set(k, copyValue(value).Interface().(T))
	}
}

// copyValue deep-copies v.
func copyValue(v reflect.Value) reflect.Value {
	out := reflect.New(v.Type()).Elem()
	switch v.Kind() {
	case reflect.Pointer:
		if !v.IsNil() {
			p := reflect.New(v.Type().Elem())
			p.Elem().Set(copyValue(v.Elem()))
			out.Set(p)
		}
	case reflect.Struct:
		for i := range v.NumField() {
			if v.Type().Field(i).IsExported() {
				out.Field(i).Set(copyValue(v.Field(i)))
			}
		}
	case reflect.Slice:
		if !v.IsNil() {
			s := reflect.MakeSlice(v.Type(), v.Len(), v.Len())
			for i := range v.Len() {
				s.Index(i).Set(copyValue(v.Index(i)))
			}
			out.Set(s)
		}
	case reflect.Map:
		if !v.IsNil() {
			m := reflect.MakeMapWithSize(v.Type(), v.Len())
			for _, k := range v.MapKeys() {
				m.SetMapIndex(k, copyValue(v.MapIndex(k)))
			}
			out.Set(m)
		}
	default:
		out.Set(v)
	}
	return out
}

func deepCopy[T any](v *T) *T { return copyValue(reflect.ValueOf(v)).Interface().(*T) }
