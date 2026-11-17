package process

import (
	"os"
	"regexp"
	"strings"
	"testing"
)

// Every preset parses and validates clean under the ceiling it is meant for:
// solo-pair and reviewed-team under the default ceiling; factory only where
// the person raised the ceiling to 5 and the Runner isolates builders.
func TestPresetsLoadAndValidate(t *testing.T) {
	factoryCeiling := []byte("version: 1\nceiling:\n  autonomy: 5\n  budgets:\n    mission: {usd: 400, wall: 48h}\n    task: {usd: 25, turns: 200, retries: 3}\n")
	for _, tc := range []struct {
		preset    string
		user      []byte
		isolation string
		autonomy  int
	}{
		{SoloPair, nil, "", 3},
		{ReviewedTeam, nil, "", 4},
		{Factory, factoryCeiling, "container", 5},
	} {
		t.Run(tc.preset, func(t *testing.T) {
			var user *File
			if tc.user != nil {
				user = Parse(LayerUser, "user.yaml", tc.user)
			}
			e := Resolve(user, nil, Options{Preset: tc.preset, Isolation: tc.isolation})
			if !e.Valid() {
				t.Fatalf("preset %s is invalid: %v", tc.preset, e.Issues.Err())
			}
			for _, i := range e.Issues {
				t.Errorf("unexpected issue: %s", i)
			}
			if e.Process.Name != tc.preset || e.Process.Autonomy != tc.autonomy || e.Preset != tc.preset {
				t.Fatalf("resolved %s autonomy %d, want %s %d", e.Process.Name, e.Process.Autonomy, tc.preset, tc.autonomy)
			}
			if !strings.HasPrefix(e.Digest, "sha256:") {
				t.Fatalf("no digest: %q", e.Digest)
			}
		})
	}
}

// The factory preset is refused where it would be unsafe, with the reason:
// above the default ceiling, and on a Runner without container isolation.
func TestFactoryIsRefusedWithoutCeilingOrIsolation(t *testing.T) {
	e := Resolve(nil, nil, Options{Preset: Factory})
	if e.Valid() {
		t.Fatal("factory validated under the default ceiling without isolation")
	}
	want := map[string]string{
		"autonomy":           "above your ceiling (4)",
		"requires.isolation": "cannot isolate builders",
	}
	for path, text := range want {
		if !hasIssue(e.Issues, path, text) {
			t.Errorf("missing %s: %q in\n%v", path, text, e.Issues.Err())
		}
	}
}

// The presets are ORCHESTRATION.md's YAML blocks verbatim, so the design
// and the shipped defaults cannot drift.
func TestPresetsMatchTheDesignDocument(t *testing.T) {
	doc, err := os.ReadFile("../../../../../ORCHESTRATION.md")
	if err != nil {
		t.Fatal(err)
	}
	blocks := regexp.MustCompile("(?s)### Preset: [^\n]+\n.*?```yaml\n(.*?)```").FindAllSubmatch(doc, -1)
	if len(blocks) != len(Presets()) {
		t.Fatalf("found %d preset blocks in ORCHESTRATION.md, want %d", len(blocks), len(Presets()))
	}
	for i, name := range Presets() {
		shipped, err := PresetSource(name)
		if err != nil {
			t.Fatal(err)
		}
		if string(blocks[i][1]) != string(shipped) {
			t.Errorf("presets/%s.yaml differs from its block in ORCHESTRATION.md; change both together", name)
		}
	}
}

func TestDefaultPresetSwitchesToReviewedTeamForSeveralTasks(t *testing.T) {
	for tasks, want := range map[int]string{0: SoloPair, 1: SoloPair, 2: ReviewedTeam, 9: ReviewedTeam} {
		if got := DefaultPreset(tasks); got != want {
			t.Errorf("DefaultPreset(%d) = %s, want %s", tasks, got, want)
		}
		if e := Resolve(nil, nil, Options{Tasks: tasks}); e.Preset != want || !e.Valid() {
			t.Errorf("Resolve with %d tasks: preset %s valid %v, want %s", tasks, e.Preset, e.Valid(), want)
		}
	}
}

func hasIssue(issues Issues, path, text string) bool {
	for _, i := range issues {
		if i.Path == path && strings.Contains(i.Message, text) {
			return true
		}
	}
	return false
}
