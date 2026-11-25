package main

import (
	"os/exec"
	"strings"
	"testing"

	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
)

// An agent links a pull request it opened, and lists what its workspace
// delivers, with the credential and endpoint Lazybox gave it: a development
// build's CLI (Dev, Working) never answers with the refusal of `lazybox link`
// for putting a release's CLI on PATH.
func TestAgentLinksAndListsItsPullRequests(t *testing.T) {
	r := newTestRunner(t)
	session, native := r.open("agent")
	env := map[string]string{}
	for _, entry := range native.Open.Env {
		if name, value, ok := strings.Cut(entry, "="); ok && strings.HasPrefix(name, "LAZYBOX_V2_") {
			env[name] = value
		}
	}
	if env["LAZYBOX_V2_ENDPOINT"] == "" || env["LAZYBOX_V2_SESSION_ID"] == "" {
		t.Fatalf("agent environment: %v", env)
	}

	// The workspace's repository comes from acme/platform: an agent
	// delivers only a pull request of a repository its members come from.
	source := r.bench.Collections.Repositories()[0].Path
	if raw, err := exec.Command("git", "-C", source, "remote", "add", "origin", "https://github.com/acme/platform.git").CombinedOutput(); err != nil {
		t.Fatalf("git remote add: %v %s", err, raw)
	}
	out := r.cli(env, "", "link", "https://github.com/acme/platform/pull/1121")
	if out.code != 0 || !strings.Contains(out.stdout, "Tracking acme/platform#1121 in this workspace") {
		t.Fatalf("link: %+v", out)
	}
	out = r.cli(env, "", "link", "https://github.com/someone/else/pull/5")
	if out.code != 0 || !strings.Contains(out.stdout, "Listed (not tracked) someone/else#5") {
		t.Fatalf("link to a repository the workspace is not: %+v", out)
	}
	// Without its scheme, after a flag, as a mention.
	out = r.cli(env, "", "link", "--role", "mentions", "github.com/acme/platform/issues/7")
	if out.code != 0 || !strings.Contains(out.stdout, "Listed (not tracked) acme/platform#7") {
		t.Fatalf("link a mention: %+v", out)
	}
	links := r.bench.WorkItems().Links(workitems.Filter{Workspace: r.collection.Id})
	if len(links) != 3 {
		t.Fatalf("links: %+v", links)
	}
	for _, l := range links {
		if l.Session != session.Id || l.Source != workitems.SourceCLI {
			t.Fatalf("linked as %+v", l)
		}
	}

	out = r.cli(env, "", "items")
	if out.code != 0 || !strings.Contains(out.stdout, "acme/platform#1121  not read yet") || !strings.Contains(out.stdout, "https://github.com/acme/platform/pull/1121") || !strings.Contains(out.stdout, "Mentioned (not tracked):") {
		t.Fatalf("items: %+v", out)
	}
	// Why it is not read yet is said.
	if !strings.Contains(out.stderr, "Connect GitHub") {
		t.Fatalf("items without GitHub: %+v", out)
	}

	out = r.cli(env, "", "items", "--all")
	if out.code != 0 || !strings.Contains(out.stdout, "acme/platform#1121") || !strings.Contains(out.stderr, "current workspace") {
		t.Fatalf("all items visible to the agent: %+v", out)
	}

	// `lazybox link` alone, in an agent, asks for the URL.
	out = r.cli(env, "", "link")
	if out.code != exitUsage || strings.Contains(out.err.Error(), "development build") || !strings.Contains(out.err.Error(), "URL") {
		t.Fatalf("link alone: %+v", out)
	}
	// Another workspace is out of an agent's reach.
	if out = r.cli(env, "", "link", "https://github.com/acme/platform/pull/9", "--workspace", "Other"); out.code == 0 {
		t.Fatalf("another workspace: %+v", out)
	}
}

func TestItemURLArgumentsAreToldFromLinkingTheCLI(t *testing.T) {
	for _, args := range [][]string{
		{"https://github.com/a/b/pull/1"},
		{"--json", "https://github.com/a/b/pull/1"},
		{"github.com/a/b/pull/1"},
		{"www.github.com/a/b/pull/1"},
		{"--role", "delivers", "x"},
		{"--workspace=W", "https://github.com/a/b/pull/1"},
	} {
		if !looksLikeItemURL(args) {
			t.Errorf("%v links the CLI", args)
		}
	}
	for _, args := range [][]string{{}, {"--dir", "/usr/local/bin"}, {"--shell", "zsh", "--force"}, {"--dry-run"}} {
		if looksLikeItemURL(args) {
			t.Errorf("%v links an item", args)
		}
	}
	for in, want := range map[string]string{
		"github.com/a/b/pull/1":             "https://github.com/a/b/pull/1",
		"http://github.com/a/b/pull/1":      "https://github.com/a/b/pull/1",
		"https://www.github.com/a/b/pull/1": "https://github.com/a/b/pull/1",
		" https://github.com/a/b/pull/1 ":   "https://github.com/a/b/pull/1",
	} {
		if got := itemURL(in); got != want {
			t.Errorf("%q → %q, want %q", in, got, want)
		}
	}
}

// What GitHub wrote reaches the terminal as one line of plain text: no escape
// sequence, carriage return or bidirectional override from a pull request's
// title or a check's name.
func TestPrintableDropsControlCharacters(t *testing.T) {
	for in, want := range map[string]string{
		"Fix refunds":                      "Fix refunds",
		"evil\x1b]0;pwned\x07\x1b[2Jtitle": "evil]0;pwned[2Jtitle",
		"ok\rFAKE":                         "okFAKE",
		"a\nb\tc\u2028d\u2029e":            "a b c d e",
		"\u202eexe.txt\u2066x\u2069":       "exe.txtx",
		"c1\u009bcontrol\u0085":            "c1control",
		"√ unicode ✕ stays":                "√ unicode ✕ stays",
	} {
		if got := printable(in); got != want {
			t.Errorf("printable(%q) = %q, want %q", in, got, want)
		}
	}
}
