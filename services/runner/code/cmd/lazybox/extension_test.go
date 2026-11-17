package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// `lazybox extension`: new writes a starter (or an example) in the Runner's
// folder and never replaces one; validate reports by file:line and fails on
// an extension that does not load; list says what the person approved.
func TestExtensionVerbs(t *testing.T) {
	r := newTestRunner(t)
	dir := filepath.Join(filepath.Dir(r.bench.Actions().Path()), "extensions")
	if out := r.cli(r.owner(), "", "extension", "path"); strings.TrimSpace(out.stdout) != dir {
		t.Fatalf("path: %+v", out)
	}
	if out := r.cli(r.owner(), "", "extension", "list"); out.code != 0 || !strings.Contains(out.stdout, "No extensions") {
		t.Fatalf("list nothing: %+v", out)
	}
	if out := r.cli(r.owner(), "", "extension", "validate"); out.code != 0 || !strings.Contains(out.stdout, "No extensions yet") {
		t.Fatalf("validate nothing: %+v", out)
	}
	out := r.cli(r.owner(), "", "extension", "new", "my-panel", "--name", "My panel")
	if out.code != 0 || !strings.Contains(out.stdout, filepath.Join(dir, "my-panel")) || !strings.Contains(out.stdout, "Settings → Extensions") {
		t.Fatalf("new: %+v", out)
	}
	for _, f := range []string{"my-panel/extension.yaml", "my-panel/index.html", "my-panel/app.js", "my-panel/lazybox.d.ts", "extension.schema.json"} {
		if _, err := os.Stat(filepath.Join(dir, f)); err != nil {
			t.Error(err)
		}
	}
	if out := r.cli(r.owner(), "", "extension", "new", "my-panel"); out.code != exitUsage || !strings.Contains(out.err.Error(), "never replaced") {
		t.Fatalf("replaced: %+v", out)
	}
	if out := r.cli(r.owner(), "", "extension", "new", "board", "--example", "agent-bord"); out.code != exitUsage || !strings.Contains(out.err.Error(), `did you mean "agent-board"`) {
		t.Fatalf("example typo: %+v", out)
	}
	if out := r.cli(r.owner(), "", "extension", "new", "agent-board", "--example", "agent-board"); out.code != 0 {
		t.Fatalf("example: %+v", out)
	}
	if out := r.cli(r.owner(), "", "extension", "validate"); out.code != 0 || !strings.Contains(out.stdout, "✓ Agent board — 1 panel; asks for read:attention, show:conversations, do:attention.next, do:inbox.open") || !strings.Contains(out.stdout, "✓ My panel") {
		t.Fatalf("validate: %+v", out)
	}
	// A mistake, by file and line.
	manifest := filepath.Join(dir, "my-panel", "extension.yaml")
	data, _ := os.ReadFile(manifest)
	if err := os.WriteFile(manifest, []byte(strings.Replace(string(data), "  - read:attention", "  - read:attenton", 1)), 0o600); err != nil {
		t.Fatal(err)
	}
	out = r.cli(r.owner(), "", "extension", "validate", "my-panel")
	if out.code != exitUsage || !strings.Contains(out.stdout, manifest+":") || !strings.Contains(out.stdout, `did you mean "read:attention"`) {
		t.Fatalf("validate a mistake: %+v", out)
	}
	if out := r.cli(r.owner(), "", "extension", "list"); out.code != 0 || !strings.Contains(out.stdout, "needs approval") || !strings.Contains(out.stdout, "does not load") || !strings.Contains(out.stdout, "read:attenton") {
		t.Fatalf("list: %+v", out)
	}
}
