package process

import (
	"strings"
	"testing"
	"time"
)

func userFile(t *testing.T, yaml string) *File {
	t.Helper()
	f := Parse(LayerUser, "user/process.yaml", []byte(yaml))
	if err := f.Issues.Err(); err != nil {
		t.Fatal(err)
	}
	return f
}

// A repository file cannot raise autonomy, permissions, providers or budgets
// above the user's ceiling. Each attempt is an error at the repository line
// that asked, never a silent clamp.
func TestCeilingBindsTheRepository(t *testing.T) {
	strict := "version: 1\nceiling:\n  autonomy: 3\n  permission: ask\n  providers: [claude]\n  budgets:\n    mission: {usd: 30}\n    task: {usd: 10, turns: 100, retries: 1}\n"
	for _, tc := range []struct {
		name, user, repo string
		path, text       string
		line             int
	}{
		{"autonomy over the default ceiling", "", "version: 1\nautonomy: 5\nrequires: {isolation: container}\n", "autonomy", "above your ceiling (4)", 2},
		{"preset over the default ceiling", "", "version: 1\nextends: factory\n", "autonomy", "above your ceiling (4)", 0},
		{"autonomy over the user's", strict, "version: 1\nextends: reviewed-team\n", "autonomy", "above your ceiling (3)", 0},
		{"permission", strict, "version: 1\nroles:\n  worker: {permission: yolo}\n", "roles.worker.permission", "yolo is above your ceiling (ask)", 3},
		{"provider", strict, "version: 1\nroles:\n  worker: {provider: codex}\n", "roles.worker.provider", "codex is not allowed by your ceiling (claude)", 3},
		{"provider in a list", strict, "version: 1\nroles:\n  worker: {provider: [claude, codex]}\n", "roles.worker.provider[1]", "not allowed by your ceiling", 3},
		{"mission budget", strict, "version: 1\nbudgets:\n  mission: {usd: 100}\n", "budgets.mission.usd", "100 is above your ceiling (30)", 3},
		{"task retries", strict, "version: 1\nbudgets:\n  task: {retries: 5}\n", "budgets.task.retries", "5 is above your ceiling (1)", 3},
		{"wall time", "", "version: 1\nbudgets:\n  mission: {wall: 24h}\n", "budgets.mission.wall", "24h is above your ceiling (10h)", 3},
		{"role turns", "", "version: 1\nroles:\n  worker: {budget: {turns: 500}}\n", "roles.worker.budget.turns", "500 is above your ceiling (120)", 3},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var user *File
			if tc.user != "" {
				user = userFile(t, tc.user)
			}
			e := Resolve(user, Parse(LayerRepository, RepositoryFile, []byte(tc.repo)), Options{Isolation: "container"})
			for _, i := range e.Issues.Errors() {
				if i.Path == tc.path && strings.Contains(i.Message, tc.text) {
					if tc.line != 0 && (i.Layer != "repository" || i.Line != tc.line) {
						t.Fatalf("attributed to %s line %d, want repository line %d", i.Layer, i.Line, tc.line)
					}
					return
				}
			}
			t.Fatalf("want %s: %q, got\n%v", tc.path, tc.text, e.Issues.Err())
		})
	}
}

// The person may raise their own ceiling; then a repository may ask for more.
func TestUserRaisesTheirCeiling(t *testing.T) {
	user := userFile(t, "version: 1\nceiling:\n  autonomy: 5\n  budgets:\n    mission: {usd: 400, wall: 48h}\n    task: {usd: 25, turns: 200, retries: 3}\n")
	e := Resolve(user, Parse(LayerRepository, RepositoryFile, []byte("version: 1\nextends: factory\n")), Options{Isolation: "container"})
	if !e.Valid() {
		t.Fatal(e.Issues.Err())
	}
	if e.Ceiling.Autonomy != 5 || e.Ceiling.Permission != "yolo" || e.Ceiling.Budgets.Mission.USD != 400 || e.Process.Ceiling != nil {
		t.Fatalf("ceiling %+v; process ceiling %+v", e.Ceiling, e.Process.Ceiling)
	}
}

// Layering: preset < user < repository, field by field; lists replace;
// roles and gates merge by name; null removes a role; unset budgets take the
// ceiling's values.
func TestLayering(t *testing.T) {
	user := userFile(t, "version: 1\nextends: solo-pair\nroles:\n  worker: {model: opus, effort: high}\n  coordinator: {model: opus}\n")
	e := Resolve(user, nil, Options{})
	if !e.Valid() || e.Preset != SoloPair {
		t.Fatalf("preset %s: %v", e.Preset, e.Issues.Err())
	}
	worker, _ := e.Process.Roles.Get("worker")
	if worker.Model != "opus" || worker.Effort != "high" || worker.Permission != "ask" || worker.Budget.Turns != 80 {
		t.Fatalf("user defaults not overlaid field by field: %+v", worker)
	}
	if _, ok := e.Process.Roles.Get("coordinator"); ok || !hasIssue(e.Issues, "roles.coordinator", "preset solo-pair has no coordinator role") {
		t.Fatalf("a user default for a role the preset lacks must be skipped with a warning: %v", e.Issues)
	}
	if b := e.Process.Budgets; b.Mission.USD != 20 || b.Mission.Wall != Duration(10*time.Hour) || b.Task.Turns != 120 || b.OnExhausted != "escalate" {
		t.Fatalf("budgets not filled from the ceiling: mission %+v task %+v", b.Mission, b.Task)
	}

	// The repository's preset choice wins; it adds a role, removes another,
	// replaces a list, and overrides one field of a gate.
	repository := Parse(LayerRepository, RepositoryFile, []byte(`version: 1
extends: reviewed-team
gates:
  accepted: {show: [evidence]}
roles:
  transition: null
  scribe:
    provider: claude
    scopes: [status.report]
    triggers: [{on: phase.entered}]
notifications:
  - {on: escalation.raised, to: [desktop]}
`))
	e = Resolve(user, repository, Options{})
	if !e.Valid() || e.Preset != ReviewedTeam {
		t.Fatalf("preset %s: %v", e.Preset, e.Issues.Err())
	}
	got := strings.Join(e.Process.Roles.Keys, ",")
	if got != "coordinator,worker,verifier,reviewer,scribe" {
		t.Fatalf("roles in order: %s", got)
	}
	if worker, _ = e.Process.Roles.Get("worker"); worker.Model != "opus" || worker.Permission != "yolo" {
		t.Fatalf("user default lost under the repository: %+v", worker)
	}
	// User defaults apply to whichever preset was chosen: reviewed-team has
	// a coordinator, so this time the user's model for it applies.
	if coordinator, _ := e.Process.Roles.Get("coordinator"); coordinator.Model != "opus" || coordinator.Effort != "high" {
		t.Fatalf("user default for coordinator: %+v", coordinator)
	}
	if accepted, _ := e.Process.Gates.Get("accepted"); accepted.Kind != "human" || strings.Join(accepted.Show, ",") != "evidence" {
		t.Fatalf("gate overlay: %+v", accepted)
	}
	if len(e.Process.Notifications) != 1 || e.Process.Notifications[0].To[0] != "desktop" {
		t.Fatalf("notifications should be replaced: %+v", e.Process.Notifications)
	}
	if scribe, _ := e.Process.Roles.Get("scribe"); scribe.Permission != "read_only" {
		t.Fatalf("permission defaults to read_only: %+v", scribe)
	}
	kinds := []string{}
	for _, l := range e.Layers {
		kinds = append(kinds, string(l.Kind)+":"+l.Source)
	}
	if strings.Join(kinds, " ") != "preset:reviewed-team user:user/process.yaml repository:"+RepositoryFile {
		t.Fatalf("layers: %v", kinds)
	}
}

// The coordinator default is applied when the preset the user overlays has
// the role. Order of layers is fixed regardless of who chose the preset.
func TestUserDefaultsApplyToTheChosenPreset(t *testing.T) {
	user := userFile(t, "version: 1\nroles:\n  coordinator: {model: opus}\n")
	e := Resolve(user, Parse(LayerRepository, RepositoryFile, []byte("version: 1\nextends: reviewed-team\n")), Options{})
	if coordinator, _ := e.Process.Roles.Get("coordinator"); !e.Valid() || coordinator.Model != "opus" {
		t.Fatalf("coordinator %+v: %v", coordinator, e.Issues.Err())
	}
}

// Issues on values a layer did not change are attributed to the layer that
// wrote them; a broken layer stops resolution with its own issues.
func TestIssuesAreAttributedToTheirLayer(t *testing.T) {
	e := Resolve(nil, Parse(LayerRepository, RepositoryFile, []byte("version: 1\nroles:\n  scribe: {provider: claude, scopes: [nope]}\n")), Options{})
	if !hasIssue(e.Issues, "roles.scribe.scopes[0]", "unknown scope") {
		t.Fatalf("issues: %v", e.Issues)
	}
	e = Resolve(nil, Parse(LayerRepository, RepositoryFile, []byte("version: 1\nautonomy: [3]\n")), Options{})
	if e.Process != nil || e.Valid() || len(e.Issues) != 1 || e.Issues[0].Line != 2 {
		t.Fatalf("a broken layer must stop resolution with its issue: %+v", e.Issues)
	}
	// Unreachable phases from a preset's phases with the repository's rules:
	// the phase is the preset's, the problem is the repository's rules.
	e = Resolve(nil, Parse(LayerRepository, RepositoryFile, []byte("version: 1\nrules: []\n")), Options{})
	for _, i := range e.Issues.Errors() {
		if i.Path == "phases[1]" && i.Layer == "preset:solo-pair" && i.Line > 0 {
			return
		}
	}
	t.Fatalf("issues: %v", e.Issues)
}

// The digest is stable for equal inputs and changes with the process.
func TestDigest(t *testing.T) {
	a := Resolve(nil, nil, Options{})
	b := Resolve(nil, nil, Options{})
	c := repo(t, "version: 1\nautonomy: 4\n", Options{})
	if a.Digest != b.Digest || a.Digest == c.Digest || !c.Valid() {
		t.Fatalf("digests %s %s %s", a.Digest, b.Digest, c.Digest)
	}
	f := Parse(LayerRepository, RepositoryFile, []byte("version: 1\n"))
	if !strings.HasPrefix(f.Digest, "sha256:") || len(f.Digest) != len("sha256:")+64 {
		t.Fatalf("file digest %s", f.Digest)
	}
}

func TestOversizedFileIsRefused(t *testing.T) {
	f := Parse(LayerRepository, RepositoryFile, []byte("version: 1\n# "+strings.Repeat("x", MaxFileBytes)+"\n"))
	if f.Process != nil || !strings.Contains(f.Issues.Err().Error(), "the limit is") {
		t.Fatalf("issues: %v", f.Issues)
	}
}

// The effective process written as YAML is itself a valid process file that
// resolves to the same process: `lazybox process show` output can be committed.
func TestEffectiveYAMLRoundTrips(t *testing.T) {
	for _, preset := range []string{SoloPair, ReviewedTeam} {
		e := Resolve(nil, nil, Options{Preset: preset})
		data, err := e.Process.YAML()
		if err != nil {
			t.Fatal(err)
		}
		again := Resolve(nil, Parse(LayerRepository, RepositoryFile, data), Options{})
		if !again.Valid() || again.Digest != e.Digest {
			t.Fatalf("%s round trip: digest %s vs %s: %v\n%s", preset, again.Digest, e.Digest, again.Issues.Err(), data)
		}
	}
}
