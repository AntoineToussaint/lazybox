package process

import (
	"embed"
	"fmt"
	"slices"
)

// The built-in presets, one per autonomy level. They are the YAML blocks of
// ORCHESTRATION.md §4 verbatim; a test keeps the two identical.
//
//go:embed presets/*.yaml
var presetFiles embed.FS

const (
	SoloPair     = "solo-pair"
	ReviewedTeam = "reviewed-team"
	Factory      = "factory"
)

// Presets lists the built-in preset names, by autonomy level.
func Presets() []string { return []string{SoloPair, ReviewedTeam, Factory} }

// DefaultPreset is the preset used when no layer extends one: solo pair, and
// reviewed team once a mission has more than one task.
func DefaultPreset(tasks int) string {
	if tasks > 1 {
		return ReviewedTeam
	}
	return SoloPair
}

// PresetSource returns a preset's YAML as shipped.
func PresetSource(name string) ([]byte, error) {
	if !slices.Contains(Presets(), name) {
		return nil, fmt.Errorf("unknown preset %q; the presets are %v", name, Presets())
	}
	return presetFiles.ReadFile("presets/" + name + ".yaml")
}

// Preset parses a built-in preset.
func Preset(name string) (*File, error) {
	data, err := PresetSource(name)
	if err != nil {
		return nil, err
	}
	f := Parse(LayerPreset, name, data)
	if err = f.Issues.Err(); err != nil {
		return nil, err // a broken preset is a build defect; tests catch it
	}
	return f, nil
}
