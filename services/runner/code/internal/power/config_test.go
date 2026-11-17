package power

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// issueAt finds the issue on a line whose message contains text.
func issueAt(issues []Issue, line int, text string) bool {
	for _, i := range issues {
		if i.Line == line && strings.Contains(i.Message, text) {
			return true
		}
	}
	return false
}

func TestManifestIsTheRegistry(t *testing.T) {
	m := Registry()
	if len(m.Actions) < 80 {
		t.Fatalf("only %d actions", len(m.Actions))
	}
	for _, id := range []string{"palette.open", "layout.cycle", "layout.maximize", "settings.open"} {
		if _, ok := m.Action(id); !ok {
			t.Fatalf("no %s", id)
		}
	}
	if a, _ := m.Action("layout.maximize"); !a.Safe {
		t.Fatal("layout.maximize should be safe for hooks")
	}
	for _, id := range []string{"settings.open", "chat.stop", "code.run-last", "workspace.new", "layout.reset-all", "chat.permissions"} {
		if a, _ := m.Action(id); a.Safe {
			t.Fatalf("%s must not be safe for hooks", id)
		}
	}
	if m.Keymap("vim")["layout.cycle"] == "" || m.Keymap("tmux")["snippets.open"] != "Ctrl+b s" {
		t.Fatalf("keymaps: %q %q", m.Keymap("vim")["layout.cycle"], m.Keymap("tmux")["snippets.open"])
	}
	if !m.ValidPanel("chat") || m.ValidPanel("nonsense") {
		t.Fatal("panels")
	}
}

func TestKeysParseByLineWithConflictsAndPresets(t *testing.T) {
	file := `# my keys
version: 1
preset: tmux
keys:
  global:
    palette.open: [Mod+k, "Ctrl+b p"]
    palete.open: Mod+y
    inbox.open: Mod+k
    layout.reset-all: Mod+r
    attention.next: Hyper+j
    files.find: []
    voice.dictate: "] ] v"
  navigation:
    navigation.next: ArrowDown
    chat.send: Mod+Shift+Enter
  sidebar:
    x: y
leader-menu:
  - {key: f, label: Fix, snippet: Fix the tests., mode: send}
  - {key: s, label: Taken, snippet: x}
  - {key: g, snippet: ""}
vim: {message-box: true}
extra: 1
`
	keys, issues, err := ParseKeys([]byte(file))
	if err != nil {
		t.Fatal(err)
	}
	if keys.Preset != "tmux" || !keys.Vim.MessageBox || keys.Vim.FileEditor {
		t.Fatalf("preset/vim: %+v", keys)
	}
	for _, want := range []struct {
		line int
		text string
	}{
		{7, `no action "palete.open" (did you mean "palette.open"?)`},
		{8, `already Command palette's key`},
		{9, "cannot be changed"},
		{10, `"Hyper" is not a modifier`},
		{12, "held down"},
		{15, "belongs under agent"},
		{16, `unknown section "sidebar"`},
		{20, "Ctrl+b s is already Open snippets"},
		{21, "needs a snippet"},
		{23, `unknown section "extra"`},
	} {
		if !issueAt(issues, want.line, want.text) {
			t.Errorf("line %d: no issue with %q in %+v", want.line, want.text, issues)
		}
	}
	bound := map[string][]string{}
	for _, b := range keys.Bindings {
		bound[b.Action] = b.Keys
	}
	if strings.Join(bound["palette.open"], ",") != "Mod+k,Ctrl+b p" || bound["files.find"] == nil || len(bound["files.find"]) != 0 || bound["navigation.next"][0] != "ArrowDown" || bound["chat.send"][0] != "Mod+Shift+Enter" {
		t.Fatalf("bindings: %+v", bound)
	}
	if _, ok := bound["inbox.open"]; ok {
		t.Fatal("a conflicting binding is left out")
	}
	if len(keys.Menu) != 1 || keys.Menu[0].Key != "f" || keys.Menu[0].Line != 19 {
		t.Fatalf("menu: %+v", keys.Menu)
	}
	// Swapping two keys is not a conflict: overrides apply together.
	swapped, issues, _ := ParseKeys([]byte("keys:\n  global:\n    attention.next: Mod+k\n    palette.open: Mod+j\n"))
	if len(issues) != 0 || len(swapped.Bindings) != 2 {
		t.Fatalf("swap: %+v %+v", swapped, issues)
	}
	// Right ⌥ or Right ⌘ alone is a key held to talk, and nothing else.
	lone, issues, _ := ParseKeys([]byte("keys:\n  global:\n    voice.dictate: [AltRight, Alt+Space]\n    voice.command: MetaRight\n    palette.open: AltRight\n    inbox.open: AltLeft\n"))
	if len(lone.Bindings) != 2 || !issueAt(issues, 5, "modifier alone") || !issueAt(issues, 6, "is not a key") {
		t.Fatalf("lone modifiers: %+v %+v", lone.Bindings, issues)
	}
	// Not YAML at all: the file as a whole does not load.
	if _, _, err := ParseKeys([]byte("keys: [\n")); err == nil || !strings.Contains(err.Error(), "not valid YAML") {
		t.Fatalf("broken: %v", err)
	}
}

func TestLayoutParse(t *testing.T) {
	good := `name: Review wide
description: Changes and the file
layout:
  split: row
  sizes: [20, 40, 40]
  children:
    - split: column
      children: [{panels: [collections]}, {panels: [agents]}]
    - panels: [changes]
    - panels: [editor, chat]
`
	l, issues, err := ParseLayout([]byte(good))
	if err != nil || len(issues) != 0 {
		t.Fatalf("good: %v %+v", err, issues)
	}
	data, _ := json.Marshal(l.Layout)
	if !strings.Contains(string(data), `"sizes":[0.2,0.4,0.4]`) || !strings.Contains(string(data), `"kinds":["editor","chat"]`) || !strings.Contains(string(data), `"sizes":[0.5,0.5]`) {
		t.Fatalf("shape: %s", data)
	}
	bad := "name: Broken\nlayout:\n  split: row\n  children:\n    - panels: [chta]\n    - panels: [chat]\n      sizes: [1]\n"
	_, issues, err = ParseLayout([]byte(bad))
	if !errors.Is(err, errLayoutInvalid) || !issueAt(issues, 5, `no panel "chta" (did you mean "chat"?)`) || !issueAt(issues, 6, "a group has panels, not sizes") {
		t.Fatalf("bad: %v %+v", err, issues)
	}
}

// The layout YAML the app writes ("Copy the layout as YAML", Save) is what this
// parser reads: scripts/config-files.test.ts checks the app writes exactly this.
func TestLayoutTheAppWrites(t *testing.T) {
	data, err := os.ReadFile(filepath.Join("testdata", "review-layout.yaml"))
	if err != nil {
		t.Fatal(err)
	}
	l, issues, err := ParseLayout(data)
	if err != nil || len(issues) != 0 || l.Name != "Review" || l.Layout.Direction != "row" || len(l.Layout.Children) != 4 || l.Layout.Sizes[0] != 0.16 || l.Layout.Children[0].Children[1].Kinds[0] != "agents" {
		t.Fatalf("%v %+v %+v", err, issues, l)
	}
}

func TestSettingsParse(t *testing.T) {
	s, issues, err := ParseSettings([]byte("notifications:\n  review: true\n  dock-badge: nope\n  sound: false\n"))
	if err != nil || !s.Notifications["review"] || len(s.Notifications) != 1 || !issueAt(issues, 3, "true or false") || !issueAt(issues, 4, `unknown setting "sound"`) {
		t.Fatalf("%+v %+v %v", s, issues, err)
	}
}

func TestSettingsAppearanceParse(t *testing.T) {
	s, issues, err := ParseSettings([]byte(`appearance:
  mode: dark
  dark-theme: dark-high-contrast
  light-theme: user:paper
  accent: "#7fb0f0"
  density: compact
  interface-font: Inter, sans-serif
  code-font-size: 14
  ligatures: true
  chat-line-height: 1.4
  chat-line-width: 100
  zoom: 1.25
  min-contrast: 7
`))
	want := map[string]any{"mode": "dark", "darkTheme": "dark-high-contrast", "lightTheme": "user:paper", "accent": "#7fb0f0", "density": "compact", "uiFont": "Inter, sans-serif", "codeFontSize": 14.0, "ligatures": true, "chatLineHeight": 1.4, "chatMaxWidth": 100.0, "zoom": 1.25, "minContrast": 7.0}
	if err != nil || len(issues) != 0 || len(s.Appearance) != len(want) {
		t.Fatalf("%+v %+v %v", s.Appearance, issues, err)
	}
	for k, v := range want {
		if s.Appearance[k] != v {
			t.Fatalf("%s: %v, want %v", k, s.Appearance[k], v)
		}
	}
	// Each mistake is left out, by line; the rest applies.
	s, issues, err = ParseSettings([]byte(`appearance:
  mode: dim
  dark-theme: light
  light-theme: solarised-light
  accent: red
  density: compact
  interface-font: "Inter; } body { x"
  chat-font-size: 40
  zoom: big
  acent: "#fff"
  motion: reduce
`))
	if err != nil || len(s.Appearance) != 2 || s.Appearance["density"] != "compact" || s.Appearance["motion"] != "reduce" ||
		!issueAt(issues, 2, `"dim" is not one of system, light, dark`) ||
		!issueAt(issues, 3, `"light" is a light theme; dark-theme takes a dark one`) ||
		!issueAt(issues, 4, `did you mean "solarized-light"?`) ||
		!issueAt(issues, 5, "is not a colour") ||
		!issueAt(issues, 7, "must be a font list") ||
		!issueAt(issues, 8, "40 is outside 11 to 22") ||
		!issueAt(issues, 9, "must be a number from 0.5 to 2") ||
		!issueAt(issues, 10, `did you mean "accent"?`) {
		t.Fatalf("%+v %+v %v", s.Appearance, issues, err)
	}
	// An unquoted #colour is a YAML comment: the value is null, which is the theme's accent.
	s, issues, _ = ParseSettings([]byte("appearance:\n  accent: #7fb0f0\n"))
	if _, set := s.Appearance["accent"]; set || len(issues) != 0 {
		t.Fatalf("%+v %+v", s.Appearance, issues)
	}
}

func TestSettingsAppearanceEditsKeepComments(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "settings.yaml")
	_ = os.WriteFile(path, []byte("# mine\nnotifications:\n  review: true # keep\n"), 0o600)
	c := NewConfig(dir)
	snap, err := c.Update("settings", []Edit{{Path: []string{"appearance", "accent"}, JSON: `"#7fb0f0"`}, {Path: []string{"appearance", "chat-line-height"}, JSON: "1.4"}, {Path: []string{"appearance", "mode"}, JSON: `"dark"`}}, nil, false)
	if err != nil {
		t.Fatal(err)
	}
	doc, _ := snap.Doc("settings")
	a := doc.Value.(Settings).Appearance
	if a["accent"] != "#7fb0f0" || a["chatLineHeight"] != 1.4 || a["mode"] != "dark" || !doc.Value.(Settings).Notifications["review"] {
		t.Fatalf("%+v", doc.Value)
	}
	data, _ := os.ReadFile(path)
	if !strings.Contains(string(data), "# mine") || !strings.Contains(string(data), "# keep") || !strings.Contains(string(data), `accent: '#7fb0f0'`) && !strings.Contains(string(data), `accent: "#7fb0f0"`) {
		t.Fatalf("%s", data)
	}
	// A value the app would refuse is refused before anything is written.
	var refused *RefusedError
	if _, err = c.Update("settings", []Edit{{Path: []string{"appearance", "zoom"}, JSON: "9"}}, nil, false); !errors.As(err, &refused) || !strings.Contains(err.Error(), "outside 0.5 to 2") {
		t.Fatalf("%v", err)
	}
	// Removing a key puts that setting back to its default.
	snap, _ = c.Update("settings", []Edit{{Path: []string{"appearance", "mode"}}}, nil, false)
	doc, _ = snap.Doc("settings")
	if _, set := doc.Value.(Settings).Appearance["mode"]; set {
		t.Fatalf("%+v", doc.Value)
	}
	// The starter parses, and the schema lists every field.
	if _, issues, err := ParseSettings([]byte(Starter(KindSettings))); err != nil || len(issues) != 0 {
		t.Fatalf("starter: %v %v", issues, err)
	}
	schema := SchemaFor(KindSettings)
	for _, f := range AppearanceFields {
		if !strings.Contains(schema, `"`+f.Name+`"`) {
			t.Fatalf("schema lacks %s", f.Name)
		}
	}
}

func TestConfigReloadsKeepsLastGoodAndWatchesLayouts(t *testing.T) {
	dir := t.TempDir()
	c := NewConfig(dir)
	snap := c.Snapshot()
	keys, _ := snap.Doc("keys")
	if keys.Exists || len(snap.Docs) != 2 || snap.Revision != 1 {
		t.Fatalf("empty: %+v", snap)
	}
	if _, changed := c.Reload(); changed {
		t.Fatal("nothing changed")
	}
	write := func(name, data string) {
		t.Helper()
		_ = os.MkdirAll(filepath.Dir(filepath.Join(dir, name)), 0o700)
		if err := os.WriteFile(filepath.Join(dir, name), []byte(data), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write("keys.yaml", "keys:\n  global:\n    palette.open: Mod+y\n")
	snap, changed := c.Reload()
	keys, _ = snap.Doc("keys")
	if !changed || !keys.Exists || keys.Err != "" || keys.Value.(Keys).Bindings[0].Keys[0] != "Mod+y" {
		t.Fatalf("loaded: %+v", keys)
	}
	// A save that is not YAML keeps the last good version, marked stale.
	write("keys.yaml", "keys: [\n")
	snap, _ = c.Reload()
	keys, _ = snap.Doc("keys")
	if !keys.Stale || keys.Err == "" || keys.Value.(Keys).Bindings[0].Keys[0] != "Mod+y" {
		t.Fatalf("stale: %+v", keys)
	}
	// A layout dropped into the folder appears; a broken one reports its line; removed, it goes.
	write("layouts/wide.yaml", "name: Wide\nlayout: {panels: [chat]}\n")
	write("layouts/broken.yaml", "name: Broken\nlayout:\n  panels: [nope]\n")
	write("layouts/Not A Name.yaml", "name: x\n")
	snap, _ = c.Reload()
	wide, ok := snap.Doc("layouts/wide")
	broken, _ := snap.Doc("layouts/broken")
	if !ok || wide.Value.(Layout).Name != "Wide" || broken.Value != nil || !issueAt(broken.Issues, 3, `no panel "nope"`) || len(snap.Docs) != 4 {
		t.Fatalf("layouts: %+v", snap.Docs)
	}
	_ = os.Remove(filepath.Join(dir, "layouts", "wide.yaml"))
	snap, changed = c.Reload()
	if _, ok := snap.Doc("layouts/wide"); ok || !changed {
		t.Fatal("removed layout still listed")
	}
}

func TestUpdateKeepsCommentsAndRefusesBreakingChanges(t *testing.T) {
	dir := t.TempDir()
	c := NewConfig(dir)
	// Created from its starter, with the schemas beside it.
	snap, err := c.Update("keys", []Edit{{Path: []string{"keys", "global", "palette.open"}, JSON: `"Mod+y"`}}, nil, false)
	if err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(filepath.Join(dir, "keys.yaml"))
	if !strings.HasPrefix(string(data), "# yaml-language-server: $schema=./keys.schema.json") || !strings.Contains(string(data), "palette.open: Mod+y") {
		t.Fatalf("created:\n%s", data)
	}
	for _, schema := range []string{"keys.schema.json", "settings.schema.json", "layout.schema.json"} {
		var v map[string]any
		raw, err := os.ReadFile(filepath.Join(dir, schema))
		if err != nil || json.Unmarshal(raw, &v) != nil {
			t.Fatalf("%s: %v", schema, err)
		}
	}
	keys, _ := snap.Doc("keys")
	if keys.Value.(Keys).Bindings[0].Action != "palette.open" {
		t.Fatalf("snapshot after write: %+v", keys)
	}
	// A comment the person wrote survives an edit from the app.
	hand := "# my shortcuts\nversion: 1\nkeys:\n  global:\n    # the palette\n    palette.open: Mod+y  # muscle memory\n"
	_ = os.WriteFile(filepath.Join(dir, "keys.yaml"), []byte(hand), 0o600)
	if _, err = c.Update("keys", []Edit{{Path: []string{"keys", "global", "palette.open"}, JSON: `["Mod+u", "] ] p"]`}, {Path: []string{"vim", "message-box"}, JSON: "true"}}, nil, false); err != nil {
		t.Fatal(err)
	}
	data, _ = os.ReadFile(filepath.Join(dir, "keys.yaml"))
	for _, want := range []string{"# my shortcuts", "# the palette", "# muscle memory", `palette.open: [Mod+u, '] ] p']`, "message-box: true"} {
		if !strings.Contains(string(data), want) {
			t.Fatalf("missing %q in:\n%s", want, data)
		}
	}
	// A change that adds an error is refused, by line, and nothing is written.
	_, err = c.Update("keys", []Edit{{Path: []string{"keys", "global", "inbox.open"}, JSON: `"Mod+u"`}}, nil, false)
	var refused *RefusedError
	if !errors.As(err, &refused) || len(refused.Issues) != 1 || !strings.Contains(refused.Issues[0].Message, "already") {
		t.Fatalf("refused: %v", err)
	}
	if after, _ := os.ReadFile(filepath.Join(dir, "keys.yaml")); string(after) != string(data) {
		t.Fatal("a refused change was written")
	}
	// Removing a key brings the default back.
	if _, err = c.Update("keys", []Edit{{Path: []string{"keys", "global", "palette.open"}}}, nil, false); err != nil {
		t.Fatal(err)
	}
	if data, _ = os.ReadFile(filepath.Join(dir, "keys.yaml")); strings.Contains(string(data), "palette.open") {
		t.Fatalf("not removed:\n%s", data)
	}
	// A whole layout: checked first, then written; removal deletes it.
	if _, err = c.Update("layouts/mine", nil, []byte("name: Mine\nlayout: {panels: [chta]}\n"), false); !errors.As(err, &refused) || refused.Issues[0].Line != 2 {
		t.Fatalf("bad layout accepted: %v", err)
	}
	snap, err = c.Update("layouts/mine", nil, []byte("name: Mine\nlayout: {panels: [chat]}\n"), false)
	if _, ok := snap.Doc("layouts/mine"); err != nil || !ok {
		t.Fatalf("layout: %v %+v", err, snap.Docs)
	}
	if snap, err = c.Update("layouts/mine", []Edit{{Path: []string{"name"}, JSON: `"Renamed"`}}, nil, false); err != nil {
		t.Fatal(err)
	}
	if d, _ := snap.Doc("layouts/mine"); d.Value.(Layout).Name != "Renamed" {
		t.Fatalf("rename: %+v", d)
	}
	if snap, err = c.Update("layouts/mine", nil, nil, true); err != nil {
		t.Fatal(err)
	}
	if _, ok := snap.Doc("layouts/mine"); ok {
		t.Fatal("not removed")
	}
	if _, err = c.Update("keys", nil, nil, true); err == nil {
		t.Fatal("keys.yaml removed")
	}
	if _, err = c.Update("../escape", nil, []byte("x"), false); err == nil {
		t.Fatal("a name outside the folder was accepted")
	}
	// A file that is not YAML now: edits are refused until it is fixed.
	_ = os.WriteFile(filepath.Join(dir, "settings.yaml"), []byte("notifications: [\n"), 0o600)
	if _, err = c.Update("settings", []Edit{{Path: []string{"notifications", "review"}, JSON: "true"}}, nil, false); !errors.As(err, &refused) || !strings.Contains(err.Error(), "fix it first") {
		t.Fatalf("broken file edited: %v", err)
	}
}

func TestDoStepAndSchema(t *testing.T) {
	f, issues, err := Parse([]byte("actions:\n  - id: wide\n    steps:\n      - do: layout.maximize\n      - do: {action: settings.open, argument: keyboard}\n  - id: bad\n    steps:\n      - do: layout.maximise\n  - id: arg\n    steps:\n      - do: {action: layout.maximize, argument: x}\n"))
	if err != nil || len(f.Actions) != 1 || f.Actions[0].Steps[0].Do != "layout.maximize" || f.Actions[0].Steps[1].DoArgument != "keyboard" {
		t.Fatalf("%v %+v", err, f)
	}
	if !issueAt(issues, 8, `(did you mean "layout.maximize"?)`) || !issueAt(issues, 11, "takes no argument") {
		t.Fatalf("issues: %+v", issues)
	}
	var schema map[string]any
	if err := json.Unmarshal([]byte(Schema), &schema); err != nil {
		t.Fatal(err)
	}
	enum := schema["$defs"].(map[string]any)["appAction"].(map[string]any)["enum"].([]any)
	if len(enum) != len(Registry().Actions) {
		t.Fatalf("schema names %d actions", len(enum))
	}
}
