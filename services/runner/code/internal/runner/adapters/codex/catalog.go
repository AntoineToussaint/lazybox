package codex

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strings"
)

// Only names from the native catalog are displayed. Paths, tool schemas,
// URLs, config and other provider metadata do not enter the journal.
func listProviderCatalog(ctx context.Context, p caller, thread, cwd, section string) (string, error) {
	type entry struct {
		Name         string `json:"name"`
		Enabled      bool   `json:"enabled"`
		Installed    bool   `json:"installed"`
		IsEnabled    bool   `json:"isEnabled"`
		IsAccessible bool   `json:"isAccessible"`
	}
	type group struct {
		Skills  []entry `json:"skills"`
		Plugins []entry `json:"plugins"`
	}
	method := map[string]string{"skills": "skills/list", "plugins": "plugin/list", "apps": "app/list"}[section]
	if method == "" {
		return "", fmt.Errorf("unknown Codex catalog %q", section)
	}
	params := map[string]any{"cwds": []string{cwd}}
	if section == "apps" {
		params = map[string]any{"threadId": thread, "limit": 80}
	}
	raw, err := p.Call(ctx, method, params)
	if err != nil {
		return "", fmt.Errorf("could not list Codex %s: %w", section, err)
	}
	var names []string
	if section == "apps" {
		var result struct {
			Data       []entry
			NextCursor string
		}
		if err := json.Unmarshal(raw, &result); err != nil {
			return "", err
		}
		for _, app := range result.Data {
			if app.IsAccessible && app.IsEnabled {
				names = append(names, app.Name)
			}
		}
		return catalogText(section, names, result.NextCursor != ""), nil
	}
	var result struct{ Data, Marketplaces []group }
	if err := json.Unmarshal(raw, &result); err != nil {
		return "", err
	}
	for _, row := range result.Data {
		for _, skill := range row.Skills {
			if skill.Enabled {
				names = append(names, skill.Name)
			}
		}
	}
	for _, marketplace := range result.Marketplaces {
		for _, plugin := range marketplace.Plugins {
			if plugin.Installed && plugin.Enabled {
				names = append(names, plugin.Name)
			}
		}
	}
	return catalogText(section, names, false), nil
}

func catalogText(section string, names []string, more bool) string {
	slices.Sort(names)
	names = slices.Compact(names)
	if i, empty := slices.BinarySearch(names, ""); empty {
		names = slices.Delete(names, i, i+1)
	}
	if len(names) > 80 {
		more, names = true, names[:80]
	}
	if len(names) == 0 && !more {
		return "No enabled Codex " + section + " were found."
	}
	if more {
		names = append(names, "additional results omitted")
	}
	return "Codex " + section + ": " + strings.Join(names, " · ")
}
