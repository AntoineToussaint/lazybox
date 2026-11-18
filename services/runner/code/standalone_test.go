package main

import (
	"os"
	"path/filepath"
	goruntime "runtime"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/restart"
)

func getenvOf(values map[string]string) func(string) string {
	return func(k string) string { return values[k] }
}

// An installed Runner needs no flags: its state goes to the person's
// Application Support (on Linux, ${XDG_DATA_HOME:-~/.local/share}/lazybox), and it runs the Node, worker and Claude Code its
// release ships. Under launchd it offers Restart Runner and keeps agents.
func TestServeDefaultsToTheInstalledLayout(t *testing.T) {
	runtime := installation.Layout{Root: "/Applications/Lazybox.app/Contents/Resources/runtime"}
	o, err := parseStandalone([]string{"--supervised"}, getenvOf(map[string]string{"HOME": "/Users/me"}), &runtime)
	if err != nil {
		t.Fatal(err)
	}
	support := "/Users/me/Library/Application Support/Lazybox"
	if goruntime.GOOS == "linux" {
		support = "/Users/me/.local/share/lazybox"
	}
	if o.state != support+"/state" || o.config != support+"/runner.config.json" || o.worktrees != support+"/workspaces" || o.actions != "/Users/me/.config/lazybox/actions.yaml" {
		t.Fatalf("paths %+v", o)
	}
	env := o.environment()
	want := map[string]string{
		"LAZYBOX_V2_WORKER":      runtime.Worker(),
		"LAZYBOX_V2_NODE":        runtime.Node(),
		"LAZYBOX_V2_CLI_DIR":     runtime.Bin(),
		"LAZYBOX_V2_WEB_DIR":     runtime.Web(),
		"LAZYBOX_V2_KEEP_AGENTS": "1",
		restart.Environment:      support + "/restart-request.json",
	}
	for k, v := range want {
		if env[k] != v {
			t.Errorf("%s = %q, want %q", k, env[k], v)
		}
	}
	// The CLI directory agents get first on PATH holds no Node.
	if strings.HasPrefix(runtime.Node(), runtime.Bin()+"/") {
		t.Fatal("the bundled Node is in the CLI directory agents get on PATH")
	}
}

// Flags win over the environment, which wins over the installed defaults;
// an unsupervised Runner does not offer a restart nothing would perform.
func TestServeFlagsAndEnvironmentOverrideTheDefaults(t *testing.T) {
	runtime := installation.Layout{Root: "/rt"}
	state := t.TempDir()
	o, err := parseStandalone([]string{"--state", state, "--keep-agents=false"},
		getenvOf(map[string]string{"HOME": "/Users/me", "LAZYBOX_V2_WORKER": "/elsewhere/main.js"}), &runtime)
	if err != nil {
		t.Fatal(err)
	}
	env := o.environment()
	if env["LAZYBOX_V2_STATE"] != state || env["LAZYBOX_V2_WORKER"] != "/elsewhere/main.js" || env["LAZYBOX_V2_KEEP_AGENTS"] != "" || env[restart.Environment] != "" {
		t.Fatalf("%v", env)
	}
	if _, err = parseStandalone([]string{"--state", "relative"}, getenvOf(map[string]string{"HOME": "/h"}), &runtime); err == nil {
		t.Fatal("a relative state directory was accepted")
	}
	if _, err = parseStandalone(nil, getenvOf(map[string]string{"HOME": "/h"}), nil); err == nil || !strings.Contains(err.Error(), "worker") {
		t.Fatalf("a Runner outside a release started without a worker: %v", err)
	}
}

// The bundled Claude Code is used only when the release ships it.
func TestServeUsesTheBundledClaudeCodeWhenPresent(t *testing.T) {
	root := t.TempDir()
	runtime := installation.Layout{Root: root}
	o, _ := parseStandalone(nil, getenvOf(map[string]string{"HOME": "/h"}), &runtime)
	if o.claude != "" {
		t.Fatalf("named a Claude Code that is not there: %s", o.claude)
	}
	if err := os.MkdirAll(filepath.Dir(runtime.ClaudeBinary()), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(runtime.ClaudeBinary(), nil, 0o755); err != nil {
		t.Fatal(err)
	}
	if o, _ = parseStandalone(nil, getenvOf(map[string]string{"HOME": "/h"}), &runtime); o.claude != runtime.ClaudeBinary() {
		t.Fatalf("claude %q", o.claude)
	}
}

// The help assistant reads the documentation a release ships; an older
// release without it names none.
func TestServeNamesTheReleasesDocumentationWhenPresent(t *testing.T) {
	runtime := installation.Layout{Root: t.TempDir()}
	o, _ := parseStandalone(nil, getenvOf(map[string]string{"HOME": "/h"}), &runtime)
	if o.environment()["LAZYBOX_V2_DOCS_DIR"] != "" {
		t.Fatalf("named documentation that is not there: %s", o.docs)
	}
	if err := os.MkdirAll(runtime.Docs(), 0o755); err != nil {
		t.Fatal(err)
	}
	o, _ = parseStandalone(nil, getenvOf(map[string]string{"HOME": "/h"}), &runtime)
	if o.environment()["LAZYBOX_V2_DOCS_DIR"] != runtime.Docs() {
		t.Fatalf("docs %q, want %q", o.docs, runtime.Docs())
	}
}
