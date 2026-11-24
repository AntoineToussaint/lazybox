package power

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

const boardManifest = `lazybox: 1
name: Board
version: 1.0.0
panels:
  - id: board
    title: Board
    icon: columns
grants: [read:attention, show:conversations, do:inbox.open]
network: [https://api.example.com]
`

func writeExtension(t *testing.T, dir, id, manifest string) string {
	t.Helper()
	folder := filepath.Join(dir, id)
	if err := os.MkdirAll(folder, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(folder, ExtensionManifestName), []byte(manifest), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(folder, "index.html"), []byte("<!doctype html><p>hi</p>"), 0o600); err != nil {
		t.Fatal(err)
	}
	return folder
}

func extIssue(issues []Issue, path string) *Issue {
	for i := range issues {
		if issues[i].Path == path {
			return &issues[i]
		}
	}
	return nil
}

func TestParseExtension(t *testing.T) {
	m, issues, err := ParseExtension([]byte(boardManifest))
	if err != nil || len(issues) != 0 {
		t.Fatalf("%v %+v", err, issues)
	}
	if m.Name != "Board" || len(m.Panels) != 1 || m.Panels[0].Entry != "index.html" || m.Panels[0].Icon != "columns" {
		t.Fatalf("%+v", m)
	}
	if strings.Join(m.Grants, ",") != "read:attention,show:conversations,do:inbox.open" || strings.Join(m.Hosts, ",") != "https://api.example.com" {
		t.Fatalf("%+v", m)
	}
}

// Every mistake is reported by line, with what was meant when it is close.
func TestParseExtensionReportsByLine(t *testing.T) {
	_, issues, err := ParseExtension([]byte(`lazybox: 2
name: Bad
colour: red
panels:
  - id: Main
    title: Main
    icon: puzle
    entry: ../outside.html
  - id: second
    entry: page.js
grants:
  - read:attenton
  - do:settings.open
  - do:no.such
  - write:files
network:
  - http://api.example.com
  - https://api.example.com/v1
  - ftp://example.com
  - https://*.com
  - http://localhost:3000
`))
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]string{
		"lazybox":         "protocol 1",
		"colour":          "unknown key",
		"panels[0].id":    "lowercase",
		"panels[0].icon":  `did you mean "puzzle"`,
		"panels[0].entry": "no .. or hidden",
		"panels[1].entry": ".html page",
		"panels[1].title": "needs a title",
		"grants[0]":       `did you mean "read:attention"`,
		"grants[1]":       "only those that show, focus or arrange",
		"grants[2]":       "no action",
		"grants[3]":       "no grant",
		"network[0]":      "https only",
		"network[1]":      "no path",
		"network[2]":      "use https",
		"network[3]":      "wildcard",
	}
	for path, text := range want {
		i := extIssue(issues, path)
		if i == nil || !strings.Contains(i.Message, text) || i.Line == 0 {
			t.Errorf("%s: want %q, got %+v", path, text, i)
		}
	}
	if i := extIssue(issues, "network[4]"); i != nil {
		t.Errorf("a local http service is allowed: %+v", i)
	}
	if i := extIssue(issues, "colour"); i == nil || i.Severity != "warning" {
		t.Errorf("unknown keys warn: %+v", i)
	}
}

func TestCheckExtensionHost(t *testing.T) {
	for raw, want := range map[string]string{
		"https://API.example.com/":     "https://api.example.com",
		"https://api.example.com:8443": "https://api.example.com:8443",
		"https://*.example.com":        "https://*.example.com",
		"http://127.0.0.1:9000":        "http://127.0.0.1:9000",
	} {
		if got, problem := CheckExtensionHost(raw); got != want || problem != "" {
			t.Errorf("%s: %q %q", raw, got, problem)
		}
	}
	// Nothing that could widen the policy built from it.
	for _, raw := range []string{"https://a.com; script-src *", "https://a.com 'unsafe-inline'", "*", "https://user@a.com", "https://a.com/x", "data:", "https://a.com?x", "javascript:alert(1)"} {
		if got, problem := CheckExtensionHost(raw); problem == "" {
			t.Errorf("%s accepted as %q", raw, got)
		}
	}
}

func TestBundledExtensionsLoadCleanly(t *testing.T) {
	examples := ExtensionExamples()
	if len(examples) != 11 {
		t.Fatalf("examples: %+v", examples)
	}
	dir := t.TempDir()
	for _, x := range examples {
		if _, err := WriteExtension(dir, x.ID, "", x.ID); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := WriteExtension(dir, "mine", `My "board": one`, ""); err != nil {
		t.Fatal(err)
	}
	e := NewExtensions(dir, filepath.Join(t.TempDir(), "extensions.json"))
	snap := e.Snapshot()
	if len(snap.Extensions) != len(examples)+1 {
		t.Fatalf("%+v", snap.Extensions)
	}
	for _, info := range snap.Extensions {
		if info.State != ExtensionNew || len(info.Issues) != 0 || info.Err != "" {
			t.Errorf("%s: %s %v %+v", info.ID, info.State, info.Err, info.Issues)
		}
	}
	mine, _ := snap.Find("mine")
	if mine.Manifest.Name != `My "board": one` {
		t.Fatalf("the name is quoted for YAML: %q", mine.Manifest.Name)
	}
	for _, f := range []string{"mine/lazybox.d.ts", "mine/app.js", ExtensionSchemaName} {
		if _, err := os.Stat(filepath.Join(dir, f)); err != nil {
			t.Error(err)
		}
	}
	// An existing extension is never replaced.
	if _, err := WriteExtension(dir, "mine", "Other", ""); err == nil || !strings.Contains(err.Error(), "never replaced") {
		t.Fatalf("replaced: %v", err)
	}
	var schema map[string]any
	if err := json.Unmarshal([]byte(ExtensionSchema()), &schema); err != nil || !strings.Contains(ExtensionSchema(), `"do:inbox.open"`) || strings.Contains(ExtensionSchema(), `"do:settings.open"`) {
		t.Fatalf("schema: %v", err)
	}
}

// An account policy is declarative: the manifest and the switch grants say
// which providers it switches, the person approves those grants, and only an
// enabled extension switches anything, for exactly the providers approved.
func TestAccountPolicyIsWhatThePersonApproved(t *testing.T) {
	m, issues, err := ParseExtension([]byte("lazybox: 1\nname: Rotation\ngrants: [switch:codex]\naccountPolicy:\n  switchOnLimit: true\n  accounts: [work, personal]\n"))
	if err != nil || len(issues) != 0 || len(m.Panels) != 0 {
		t.Fatalf("a panel-less policy: %v %+v", err, issues)
	}
	if p := m.AccountPolicy; p == nil || !p.SwitchOnLimit || !slices.Equal(p.Providers, []string{"codex"}) || !slices.Equal(p.Accounts, []string{"work", "personal"}) {
		t.Fatalf("policy %+v", m.AccountPolicy)
	}
	for manifest, path := range map[string]string{
		"lazybox: 1\nname: R\naccountPolicy: {switchOnLimit: true}\n":                          "accountPolicy",
		"lazybox: 1\nname: R\ngrants: [switch:claude]\npanels: [{id: a, title: A}]\n":          "grants",
		"lazybox: 1\nname: R\ngrants: [switch:gemini]\naccountPolicy: {switchOnLimit: true}\n": "grants[0]",
		"lazybox: 1\nname: R\ngrants: [switch:codex]\naccountPolicy: {switchOnLimit: yes}\n":   "accountPolicy.switchOnLimit",
		"lazybox: 1\nname: R\ngrants: [switch:codex]\naccountPolicy: {accounts: [../x]}\n":     "accountPolicy.accounts",
		"lazybox: 1\nname: R\n": "panels",
	} {
		_, issues, _ := ParseExtension([]byte(manifest))
		if i := extIssue(issues, path); i == nil || i.Severity != "error" {
			t.Errorf("%q: no error at %s: %+v", manifest, path, issues)
		}
	}

	dir := t.TempDir()
	if _, err := WriteExtension(dir, "subscription-rotation", "", "subscription-rotation"); err != nil {
		t.Fatal(err)
	}
	e := NewExtensions(dir, filepath.Join(t.TempDir(), "extensions.json"))
	t.Cleanup(e.Close)
	info, _ := e.Snapshot().Find("subscription-rotation")
	if info.State != ExtensionNew || info.SwitchesFor("codex") || info.SwitchesFor("claude") {
		t.Fatalf("installed is not approved: %s", info.State)
	}
	if !slices.Equal(info.Manifest.Grants, []string{"switch:codex", "switch:claude"}) || len(info.Manifest.AccountPolicy.Accounts) != 0 {
		t.Fatalf("the bundled policy: %+v %+v", info.Manifest.Grants, info.Manifest.AccountPolicy)
	}
	snap, err := e.Approve("subscription-rotation", info.Manifest.Grants, info.Manifest.Hosts)
	if err != nil {
		t.Fatal(err)
	}
	info, _ = snap.Find("subscription-rotation")
	if !info.SwitchesFor("codex") || !info.SwitchesFor("claude") || info.SwitchesFor("gemini") {
		t.Fatal("approved policy does not switch")
	}
	snap, _ = e.SetEnabled("subscription-rotation", false)
	if info, _ = snap.Find("subscription-rotation"); info.SwitchesFor("codex") {
		t.Fatal("an extension turned off still switches")
	}
	// An edit that adds a provider is asked again; until then it switches only for what was approved.
	if _, err = e.SetEnabled("subscription-rotation", true); err != nil {
		t.Fatal(err)
	}
	folder := filepath.Join(dir, "subscription-rotation")
	if err = os.WriteFile(filepath.Join(folder, ExtensionManifestName), []byte("lazybox: 1\nname: Subscription rotation\ngrants: [switch:codex]\naccountPolicy: {switchOnLimit: true}\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	snap, _ = e.Reload()
	if info, _ = snap.Find("subscription-rotation"); !info.SwitchesFor("codex") || info.SwitchesFor("claude") {
		t.Fatal("narrowing to Codex must stop Claude switching")
	}
}

// Nothing runs until the person approves exactly what it asks for; an update
// that asks for more runs with what was approved until it is reviewed;
// sessions holding grants that no longer hold are revoked.
func TestExtensionApprovalLifecycle(t *testing.T) {
	dir, state := t.TempDir(), filepath.Join(t.TempDir(), "extensions.json")
	folder := writeExtension(t, dir, "board", boardManifest)
	e := NewExtensions(dir, state)
	t.Cleanup(e.Close)
	info, _ := e.Snapshot().Find("board")
	if info.State != ExtensionNew {
		t.Fatalf("state %s", info.State)
	}
	if _, err := e.Open("board", "", "http://127.0.0.1:5173"); err == nil || !strings.Contains(err.Error(), "not approved") {
		t.Fatalf("opened before approval: %v", err)
	}
	// What the sheet showed must be what it asks for.
	if _, err := e.Approve("board", []string{"read:attention"}, info.Manifest.Hosts); err == nil {
		t.Fatal("approved a different set")
	}
	snap, err := e.Approve("board", info.Manifest.Grants, info.Manifest.Hosts)
	if info, _ = snap.Find("board"); err != nil || info.State != ExtensionEnabled {
		t.Fatalf("%v %+v", err, info)
	}
	// Kept in the Runner's state, owner only.
	if st, err := os.Stat(state); err != nil || st.Mode().Perm() != 0o600 {
		t.Fatalf("approvals file: %v %v", st, err)
	}
	session, err := e.Open("board", "board", "http://127.0.0.1:5173")
	if err != nil || !strings.HasPrefix(session.Credential, ExtensionCredentialPrefix) || !strings.Contains(session.URL, "/_lazybox/shell.html?entry=index.html") {
		t.Fatalf("%+v %v", session, err)
	}
	if id, grants, err := e.Scope(session.Credential); err != nil || id != "board" || !slices.Equal(grants, info.Manifest.Grants) {
		t.Fatalf("scope %s %v %v", id, grants, err)
	}
	if _, _, err := e.Scope(session.Credential + "x"); !errors.Is(err, ErrExtensionCredential) {
		t.Fatal("a forged credential was accepted")
	}

	// An update asks for more: it runs with what was approved, flagged for review.
	widened := strings.Replace(boardManifest, "do:inbox.open]", "do:inbox.open, send:messages]", 1)
	if err := os.WriteFile(filepath.Join(folder, ExtensionManifestName), []byte(widened), 0o600); err != nil {
		t.Fatal(err)
	}
	snap, _ = e.Reload()
	info, _ = snap.Find("board")
	grants, _ := info.Effective()
	if info.State != ExtensionReview || slices.Contains(grants, GrantSendMessages) {
		t.Fatalf("widened: %s %v", info.State, grants)
	}
	if _, g, err := e.Scope(session.Credential); err != nil || slices.Contains(g, GrantSendMessages) {
		t.Fatalf("the open panel gained a grant: %v %v", g, err)
	}
	// Turning it back on without review is refused.
	if _, err := e.SetEnabled("board", false); err != nil {
		t.Fatal(err)
	}
	if _, _, err := e.Scope(session.Credential); err == nil {
		t.Fatal("a disabled extension's credential still works")
	}
	if _, err := e.SetEnabled("board", true); err == nil || !strings.Contains(err.Error(), "review it first") {
		t.Fatalf("enabled without review: %v", err)
	}
	// Approving the update gives the new grant to new sessions only.
	snap, err = e.Approve("board", []string{"read:attention", "show:conversations", "do:inbox.open", "send:messages"}, info.Manifest.Hosts)
	if info, _ = snap.Find("board"); err != nil || info.State != ExtensionEnabled {
		t.Fatalf("%v %+v", err, info)
	}
	second, _ := e.Open("board", "", "")
	if _, g, _ := e.Scope(second.Credential); !slices.Contains(g, GrantSendMessages) {
		t.Fatalf("approved grant missing: %v", g)
	}
	// Narrowing needs no approval, and revokes sessions holding more.
	if err := os.WriteFile(filepath.Join(folder, ExtensionManifestName), []byte(strings.Replace(boardManifest, ", do:inbox.open]", "]", 1)), 0o600); err != nil {
		t.Fatal(err)
	}
	snap, _ = e.Reload()
	if info, _ = snap.Find("board"); info.State != ExtensionEnabled {
		t.Fatalf("narrowed: %s", info.State)
	}
	if _, _, err := e.Scope(second.Credential); err == nil {
		t.Fatal("a session holding a grant the manifest dropped survived")
	}
	// The approvals survive a Runner restart.
	again := NewExtensions(dir, state)
	if info, _ = again.Snapshot().Find("board"); info.State != ExtensionEnabled {
		t.Fatalf("after restart: %s", info.State)
	}
	// Remove deletes the folder and the approval.
	if _, err := e.Remove("board"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(folder); !os.IsNotExist(err) {
		t.Fatal("folder left")
	}
	writeExtension(t, dir, "board", boardManifest)
	if snap, _ = e.Reload(); len(snap.Extensions) != 1 || snap.Extensions[0].State != ExtensionNew {
		t.Fatalf("a re-added extension kept its old approval: %+v", snap.Extensions)
	}
}

func TestBrokenExtensionNeverRuns(t *testing.T) {
	dir := t.TempDir()
	writeExtension(t, dir, "bad", "name: Bad\npanels: [{id: main, title: Main}]\ngrants: [read:everything]\n")
	writeExtension(t, dir, "Upper", boardManifest)
	linked := writeExtension(t, dir, "linked", boardManifest)
	if err := os.Symlink("/etc/hosts", filepath.Join(linked, "hosts.txt")); err != nil {
		t.Fatal(err)
	}
	missing := writeExtension(t, dir, "missing", strings.Replace(boardManifest, "icon: columns", "icon: columns\n    entry: nope.html", 1))
	_ = missing
	e := NewExtensions(dir, filepath.Join(t.TempDir(), "x.json"))
	for _, id := range []string{"bad", "Upper", "linked", "missing"} {
		info, ok := e.Snapshot().Find(id)
		if !ok || info.State != ExtensionBroken {
			t.Errorf("%s: %+v", id, info)
		}
		if _, err := e.Approve(id, info.Manifest.Grants, info.Manifest.Hosts); err == nil {
			t.Errorf("%s approved while broken", id)
		}
	}
	if info, _ := e.Snapshot().Find("linked"); !strings.Contains(info.Err, "symbolic link") {
		t.Errorf("linked: %s", info.Err)
	}
}
