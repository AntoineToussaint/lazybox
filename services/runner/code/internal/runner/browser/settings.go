package browser

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"sort"
)

// Settings → Browser, kept by the Runner so every window and every agent sees
// the same rules.
type Settings struct {
	Revision uint64 `json:"revision"`
	// DefaultPermission: ask (each new site asks), allow (any site), off (no browser).
	DefaultPermission string   `json:"defaultPermission"`
	AllowedSites      []string `json:"allowedSites,omitempty"`
	// Downloads: ask | block.
	Downloads string `json:"downloads"`
	// ShowPanel shows the agent browser beside the chat when an agent starts browsing.
	ShowPanel bool `json:"showPanel"`
	// WorkspaceSites are the sites always allowed per workspace.
	WorkspaceSites map[string][]string `json:"workspaceSites,omitempty"`
	// OffWorkspaces are the workspaces whose agents get no browser, whatever
	// DefaultPermission says (Settings → Browser, per workspace).
	OffWorkspaces []string `json:"offWorkspaces,omitempty"`
}

// ErrConflict refuses a save made against an older revision.
var ErrConflict = errors.New("browser settings changed since they were read")

// Permission and download values.
const (
	PermissionAsk   = "ask"
	PermissionAllow = "allow"
	PermissionOff   = "off"
	DownloadsAsk    = "ask"
	DownloadsBlock  = "block"
)

const maxSites = 500

// DefaultSettings: ask before each new site, ask before each download, show the panel.
func DefaultSettings() Settings {
	return Settings{Revision: 1, DefaultPermission: PermissionAsk, Downloads: DownloadsAsk, ShowPanel: true}
}

// validate normalizes a settings value the person saved.
func (s *Settings) validate() error {
	if !slices.Contains([]string{PermissionAsk, PermissionAllow, PermissionOff}, s.DefaultPermission) {
		return fmt.Errorf("default permission must be ask, allow or off, not %q", s.DefaultPermission)
	}
	if !slices.Contains([]string{DownloadsAsk, DownloadsBlock}, s.Downloads) {
		return fmt.Errorf("downloads must be ask or block, not %q", s.Downloads)
	}
	sites, err := normalizeSites(s.AllowedSites)
	if err != nil {
		return err
	}
	s.AllowedSites = sites
	for workspace, list := range s.WorkspaceSites {
		if !safeName(workspace) {
			return fmt.Errorf("invalid workspace %q", workspace)
		}
		if list, err = normalizeSites(list); err != nil {
			return err
		}
		if len(list) == 0 {
			delete(s.WorkspaceSites, workspace)
			continue
		}
		s.WorkspaceSites[workspace] = list
	}
	off := []string{}
	for _, workspace := range s.OffWorkspaces {
		if !safeName(workspace) {
			return fmt.Errorf("invalid workspace %q", workspace)
		}
		if !slices.Contains(off, workspace) {
			off = append(off, workspace)
		}
	}
	sort.Strings(off)
	s.OffWorkspaces = off
	if len(off) == 0 {
		s.OffWorkspaces = nil
	}
	return nil
}

func normalizeSites(in []string) ([]string, error) {
	if len(in) > maxSites {
		return nil, fmt.Errorf("at most %d sites", maxSites)
	}
	out := []string{}
	for _, entry := range in {
		site, err := NormalizeSite(entry)
		if err != nil {
			return nil, err
		}
		if !slices.Contains(out, site) {
			out = append(out, site)
		}
	}
	sort.Strings(out)
	return out, nil
}

// settingsStore persists Settings atomically, owner-only.
type settingsStore struct {
	path    string
	current Settings
}

func openSettings(path string) (*settingsStore, error) {
	s := &settingsStore{path: path, current: DefaultSettings()}
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return s, nil
	}
	if err != nil {
		return nil, err
	}
	var loaded Settings
	if err = json.Unmarshal(data, &loaded); err != nil {
		return nil, fmt.Errorf("browser settings %s: %w", path, err)
	}
	if err = loaded.validate(); err != nil {
		return nil, fmt.Errorf("browser settings %s: %w", path, err)
	}
	s.current = loaded
	return s, nil
}

func (s *settingsStore) get() Settings { return clone(s.current) }

// save replaces the settings when next names the current revision.
func (s *settingsStore) save(next Settings) (Settings, error) {
	if next.Revision != s.current.Revision {
		return s.get(), ErrConflict
	}
	next = clone(next)
	if err := next.validate(); err != nil {
		return s.get(), err
	}
	next.Revision = s.current.Revision + 1
	if err := writePrivate(s.path, next); err != nil {
		return s.get(), err
	}
	s.current = next
	return s.get(), nil
}

// update applies change to the current settings and saves them.
func (s *settingsStore) update(change func(*Settings)) (Settings, error) {
	next := s.get()
	change(&next)
	return s.save(next)
}

func clone(s Settings) Settings {
	s.AllowedSites = slices.Clone(s.AllowedSites)
	sites := map[string][]string{}
	for k, v := range s.WorkspaceSites {
		sites[k] = slices.Clone(v)
	}
	s.WorkspaceSites = sites
	s.OffWorkspaces = slices.Clone(s.OffWorkspaces)
	return s
}

// writePrivate writes value as JSON to path atomically, readable by the owner only.
func writePrivate(path string, value any) error {
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(path), ".browser-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), path)
}
