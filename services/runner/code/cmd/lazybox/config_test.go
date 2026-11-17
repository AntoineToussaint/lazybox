package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// `lazybox config`: the folder is the Runner's; get, set (comments kept,
// conflicts refused by line), validate by file:line, edit through $EDITOR,
// export and import a bundle.
func TestConfigVerbs(t *testing.T) {
	r := newTestRunner(t)
	dir := filepath.Dir(r.bench.Actions().Path())
	if out := r.cli(r.owner(), "", "config", "path"); out.code != 0 || strings.TrimSpace(out.stdout) != dir {
		t.Fatalf("path: %+v", out)
	}
	if out := r.cli(r.owner(), "", "config", "path", "layout", "review"); strings.TrimSpace(out.stdout) != filepath.Join(dir, "layouts", "review.yaml") {
		t.Fatalf("layout path: %+v", out)
	}
	// Without a Runner, the person's own folder.
	if out := r.cli(map[string]string{"XDG_CONFIG_HOME": "/xdg"}, "", "config", "path"); strings.TrimSpace(out.stdout) != "/xdg/lazybox" {
		t.Fatalf("no runner: %+v", out)
	}
	if out := r.cli(r.owner(), "", "config", "validate"); out.code != 0 || !strings.Contains(out.stdout, "No config files yet") {
		t.Fatalf("validate nothing: %+v", out)
	}
	// set creates the file from its starter; get reads the action's keys back.
	if out := r.cli(r.owner(), "", "config", "set", "keys", "palette.open", "Mod+y"); out.code != 0 {
		t.Fatalf("set: %+v", out)
	}
	path := filepath.Join(dir, "keys.yaml")
	data, _ := os.ReadFile(path)
	if !strings.Contains(string(data), "yaml-language-server: $schema=./keys.schema.json") || !strings.Contains(string(data), "palette.open: Mod+y") {
		t.Fatalf("keys.yaml:\n%s", data)
	}
	if out := r.cli(r.owner(), "", "config", "get", "keys", "palette.open"); out.code != 0 || !strings.HasPrefix(out.stdout, "Mod+y\t") || !strings.Contains(out.stdout, path+":") {
		t.Fatalf("get: %+v", out)
	}
	if out := r.cli(r.owner(), "", "config", "get", "keys", "inbox.open", "--json"); !strings.Contains(out.stdout, `"source": "preset vim"`) {
		t.Fatalf("get default: %+v", out)
	}
	if out := r.cli(r.owner(), "", "config", "set", "settings", "notifications.review", "true"); out.code != 0 {
		t.Fatalf("set setting: %+v", out)
	}
	if out := r.cli(r.owner(), "", "config", "get", "settings", "notifications.review"); strings.TrimSpace(out.stdout) != "true" {
		t.Fatalf("get setting: %+v", out)
	}
	// A colour typed bare is the colour, not a YAML comment that would clear it.
	if out := r.cli(r.owner(), "", "config", "set", "settings", "appearance.accent", "#7fb0f0"); out.code != 0 {
		t.Fatalf("set accent: %+v", out)
	}
	if out := r.cli(r.owner(), "", "config", "get", "settings", "appearance.accent"); strings.TrimSpace(out.stdout) != "#7fb0f0" {
		t.Fatalf("get accent: %+v", out)
	}
	if out := r.cli(r.owner(), "", "config", "set", "settings", "appearance.zoom", "3"); out.code != exitUsage || !strings.Contains(out.stdout, "outside 0.5 to 2") {
		t.Fatalf("zoom out of range: %+v", out)
	}
	// A conflict is refused by line and nothing is written.
	before, _ := os.ReadFile(path)
	out := r.cli(r.owner(), "", "config", "set", "keys", "inbox.open", "Mod+y")
	if out.code != exitUsage || !strings.Contains(out.stdout, "already Command palette's key") {
		t.Fatalf("conflict: %+v", out)
	}
	if after, _ := os.ReadFile(path); string(after) != string(before) {
		t.Fatal("a refused set was written")
	}
	if out = r.cli(r.owner(), "", "config", "set", "keys", "palette.open", "--remove"); out.code != 0 {
		t.Fatalf("remove: %+v", out)
	}
	// validate reports a hand-made mistake as file:line and fails.
	_ = os.WriteFile(path, []byte("# hand\nkeys:\n  global:\n    palete.open: Mod+y\n"), 0o600)
	out = r.cli(r.owner(), "", "config", "validate")
	if out.code != exitUsage || !strings.Contains(out.stdout, path+":4: error:") || !strings.Contains(out.stdout, `did you mean "palette.open"`) {
		t.Fatalf("validate: %+v", out)
	}
	// edit opens $EDITOR on the file (created first), then checks it.
	editor := filepath.Join(t.TempDir(), "editor.sh")
	_ = os.WriteFile(editor, []byte("#!/bin/sh\nprintf 'name: Mine\\nlayout: {panels: [chat]}\\n' > \"$1\"\n"), 0o700)
	out = r.cli(map[string]string{"LAZYBOX_V2_ENDPOINT": r.endpoint, "EDITOR": editor}, "", "config", "edit", "layout", "mine")
	if out.code != 0 || !strings.Contains(out.stdout, "mine.yaml ✓") {
		t.Fatalf("edit: %+v", out)
	}
	if _, err := os.Stat(filepath.Join(dir, "layout.schema.json")); err != nil {
		t.Fatal("no layout schema", err)
	}
	// export, then import into another folder (no Runner there).
	_ = os.WriteFile(path, []byte("# hand\nkeys:\n  global:\n    palette.open: Mod+y\n"), 0o600)
	bundleFile := filepath.Join(t.TempDir(), "bundle.yaml")
	if out = r.cli(r.owner(), "", "config", "export", "--out", bundleFile); out.code != 0 {
		t.Fatalf("export: %+v", out)
	}
	bundleText, _ := os.ReadFile(bundleFile)
	if !strings.Contains(string(bundleText), "keys.yaml: |") || !strings.Contains(string(bundleText), "# hand") || !strings.Contains(string(bundleText), "layouts/mine.yaml") || strings.Contains(string(bundleText), "actions.yaml") {
		t.Fatalf("bundle:\n%s", bundleText)
	}
	other := t.TempDir()
	env := map[string]string{"XDG_CONFIG_HOME": other}
	if out = r.cli(env, "", "config", "import", bundleFile, "--dry-run"); out.code != 0 || !strings.Contains(out.stdout, "new") {
		t.Fatalf("dry run: %+v", out)
	}
	if _, err := os.Stat(filepath.Join(other, "lazybox", "keys.yaml")); err == nil {
		t.Fatal("dry run wrote")
	}
	if out = r.cli(env, "", "config", "import", bundleFile); out.code != 0 {
		t.Fatalf("import: %+v", out)
	}
	if got, _ := os.ReadFile(filepath.Join(other, "lazybox", "keys.yaml")); !strings.Contains(string(got), "# hand") {
		t.Fatalf("imported: %s", got)
	}
	// A bundle with a broken file writes nothing.
	broken := strings.Replace(string(bundleText), "palette.open: Mod+y", "palette.open: Hyper+y", 1)
	if out = r.cli(map[string]string{"XDG_CONFIG_HOME": t.TempDir()}, broken, "config", "import", "-"); out.code != exitUsage || !strings.Contains(out.stdout, "keys.yaml:4:") {
		t.Fatalf("broken bundle: %+v", out)
	}
}

// `lazybox do`: --list prints the registry; an action goes to the window;
// a typo is not found, with a suggestion; your own actions run the same way.
func TestDoVerb(t *testing.T) {
	r := newTestRunner(t)
	out := r.cli(nil, "", "do", "--list", "layout")
	if out.code != 0 || !strings.Contains(out.stdout, "layout.maximize") || !strings.Contains(out.stdout, "safe") || strings.Contains(out.stdout, "palette.open") {
		t.Fatalf("list: %+v", out)
	}
	out = r.cli(nil, "", "do", "--list", "--json")
	var listed []struct {
		ID   string `json:"id"`
		Safe bool   `json:"safe"`
	}
	if err := json.Unmarshal([]byte(out.stdout), &listed); err != nil || len(listed) < 80 {
		t.Fatalf("json: %v %+v", err, out)
	}
	if out = r.cli(r.owner(), "", "do", "layout.maximise"); out.code != exitNotFound || !strings.Contains(out.err.Error(), "did you mean layout.maximize") {
		t.Fatalf("typo: %+v", out)
	}
	if out = r.cli(r.owner(), "", "do", "layout.maximize"); out.code != exitUnavailable {
		t.Fatalf("no window: %+v", out)
	}
	requests, stop := r.bench.UI().Watch("desktop")
	defer stop()
	seen := make(chan string, 4)
	go func() {
		for req := range requests {
			seen <- req.Kind + " " + req.ActionID + " " + req.Argument
			_, _ = r.bench.UI().Complete(req.ID, "", "Maximized the Chat panel", nil)
		}
	}()
	if out = r.cli(r.owner(), "", "do", "layout.maximize"); out.code != 0 || !strings.Contains(out.stdout, "Maximized the Chat panel") || <-seen != "do layout.maximize " {
		t.Fatalf("do: %+v", out)
	}
	if out = r.cli(r.owner(), "", "do", "settings.open", "keyboard"); out.code != 0 || <-seen != "do settings.open keyboard" {
		t.Fatalf("argument: %+v", out)
	}
	if out = r.cli(r.owner(), "", "do", "layout.maximize", "x"); out.code != exitUsage {
		t.Fatalf("argument refused: %+v", out)
	}
	_ = os.WriteFile(r.bench.Actions().Path(), []byte("actions:\n  - id: go-on\n    steps:\n      - do: layout.maximize\n"), 0o600)
	if out = r.cli(r.owner(), "", "do", "go-on"); out.code != 0 || <-seen != "action go-on " {
		t.Fatalf("user action: %+v", out)
	}
}
