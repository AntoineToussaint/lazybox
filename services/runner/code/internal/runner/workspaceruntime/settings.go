package workspaceruntime

import (
	"errors"
	"sync"
)

// Override is the person's own runtime configuration for one repository,
// kept on this Runner and never committed. Config uses the same schema as
// .lazybox/workspace.yaml; its entries add to or replace the repository's and
// need no approval: the person wrote them.
type Override struct {
	Config        string `json:"config,omitempty"`
	DefaultAction string `json:"defaultAction,omitempty"`
	SkipSetup     bool   `json:"skipSetup,omitempty"`
}

// Parsed decodes the override's configuration.
func (o Override) Parsed() (Config, error) {
	if o.Config == "" {
		return Config{Version: 1}, nil
	}
	return Parse([]byte(o.Config))
}

// Settings are the person's runtime settings.
type Settings struct {
	Revision     uint64              `json:"revision"`
	PortStart    int                 `json:"portStart"`
	PortEnd      int                 `json:"portEnd"`
	Repositories map[string]Override `json:"repositories,omitempty"`
}

// ErrStaleSettings refuses a save made against an older revision.
var ErrStaleSettings = errors.New("runtime settings changed since they were read; reload and retry")

// SettingsStore keeps Settings in one file.
type SettingsStore struct {
	mu   sync.Mutex
	path string
	s    Settings
}

// OpenSettings loads the settings kept at path, with defaults.
func OpenSettings(path string) (*SettingsStore, error) {
	st := &SettingsStore{path: path, s: Settings{Revision: 1, PortStart: DefaultPortStart, PortEnd: DefaultPortEnd}}
	if err := readJSON(path, &st.s); err != nil {
		return nil, err
	}
	if ValidRange(st.s.PortStart, st.s.PortEnd) != nil {
		st.s.PortStart, st.s.PortEnd = DefaultPortStart, DefaultPortEnd
	}
	return st, nil
}

// Get returns a copy of the settings.
func (st *SettingsStore) Get() Settings {
	st.mu.Lock()
	defer st.mu.Unlock()
	return st.getLocked()
}

// Override returns one repository's override.
func (st *SettingsStore) Override(repository string) Override {
	st.mu.Lock()
	defer st.mu.Unlock()
	return st.s.Repositories[repository]
}

// Save replaces the settings when next.Revision is the current one. An
// override whose configuration does not parse is refused with its error.
func (st *SettingsStore) Save(next Settings) (Settings, error) {
	if err := ValidRange(next.PortStart, next.PortEnd); err != nil {
		return Settings{}, err
	}
	clean := map[string]Override{}
	for id, o := range next.Repositories {
		if id == "" {
			return Settings{}, errors.New("an override names no repository")
		}
		if _, err := o.Parsed(); err != nil {
			return Settings{}, errors.New("your configuration for " + id + ": " + err.Error())
		}
		if o.Config == "" && o.DefaultAction == "" && !o.SkipSetup {
			continue
		}
		clean[id] = o
	}
	st.mu.Lock()
	defer st.mu.Unlock()
	if next.Revision != st.s.Revision {
		return Settings{}, ErrStaleSettings
	}
	next.Repositories = clean
	next.Revision = st.s.Revision + 1
	if err := writeJSON(st.path, next); err != nil {
		return Settings{}, err
	}
	st.s = next
	return st.getLocked(), nil
}

func (st *SettingsStore) getLocked() Settings {
	out := st.s
	out.Repositories = map[string]Override{}
	for k, v := range st.s.Repositories {
		out.Repositories[k] = v
	}
	return out
}
