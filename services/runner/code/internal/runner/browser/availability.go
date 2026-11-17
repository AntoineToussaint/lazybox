package browser

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
)

// Whether agents can browse on this Runner, and when they cannot, why and
// what fixes it (Settings → Browser shows it; the agents' briefing says it).

// Availability states.
const (
	// AvailabilityReady: agents opened now get the browser and it opens.
	AvailabilityReady = "ready"
	// AvailabilityNotInstalled: agents get the browser tools, but the Chromium
	// the service runs is not installed yet; Install installs it.
	AvailabilityNotInstalled = "not_installed"
	// AvailabilityOff: the person chose Off in Settings → Browser.
	AvailabilityOff = "off"
	// AvailabilityUnavailable: this Runner has no browser service (an
	// installed release, for now).
	AvailabilityUnavailable = "unavailable"
)

// Availability is the browser on this Runner as the person and agents see it.
type Availability struct {
	State string
	// Problem says, in the person's words, why agents cannot browse; empty when ready.
	Problem string
	// Install is the command that installs the Chromium the service runs: set
	// when it is missing and the service runs from a source checkout.
	Install []string
}

// Problems, shown in Settings → Browser and told to agents.
const (
	problemUnavailable  = "The agent browser is a developer-build feature for now: the installed app does not include it, so agents cannot browse. In a developer build, run make browser once, then start Lazybox with make dev or make working. The Web panel is not affected."
	problemNotInstalled = "Chromium for the agent browser isn’t installed, so the browser cannot open yet. Install it once (make browser); agents keep their browser and it works as soon as Chromium is there."
	problemOff          = "The agent browser is off: agents get no browser. Choose Ask before each new site to turn it on."
)

// Availability reports whether agents can browse now, and if not, why.
func (m *Manager) Availability() Availability {
	if !m.Available() {
		return Availability{State: AvailabilityUnavailable, Problem: problemUnavailable}
	}
	m.mu.Lock()
	off := m.settings.current.DefaultPermission == PermissionOff
	m.mu.Unlock()
	if off {
		return Availability{State: AvailabilityOff, Problem: problemOff}
	}
	env := m.cfg.Env
	if env == nil {
		env = serviceEnv(os.Environ())
	}
	if installed, known := chromiumInstalled(m.cfg.Script, env); known && !installed {
		return Availability{State: AvailabilityNotInstalled, Problem: problemNotInstalled, Install: m.installCommand()}
	}
	return Availability{State: AvailabilityReady}
}

// EnabledFor reports whether agents opened now in workspace get the browser
// tools: the Runner has the service, the person has not turned it off, and
// not for this workspace. A missing Chromium does not withhold the tools:
// installing it makes them work without restarting anyone.
func (m *Manager) EnabledFor(workspace string) bool {
	if !m.Enabled() {
		return false
	}
	return !m.WorkspaceOff(workspace)
}

// WorkspaceOff reports whether the person turned the browser off for workspace.
func (m *Manager) WorkspaceOff(workspace string) bool {
	if m == nil || workspace == "" {
		return false
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	return slices.Contains(m.settings.current.OffWorkspaces, workspace)
}

// SetWorkspaceOff turns the browser off or back on for one workspace.
func (m *Manager) SetWorkspaceOff(workspace string, off bool) (Settings, error) {
	if !safeName(workspace) {
		return Settings{}, errors.New("invalid workspace")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.settings.update(func(s *Settings) {
		s.OffWorkspaces = slices.DeleteFunc(s.OffWorkspaces, func(w string) bool { return w == workspace })
		if off {
			s.OffWorkspaces = append(s.OffWorkspaces, workspace)
		}
	})
}

// installCommand installs the service's Chromium with the playwright-core it
// runs (what `make browser` does), or nil when there is none beside it.
func (m *Manager) installCommand() []string {
	core := playwrightCore(m.cfg.Script)
	if core == "" {
		return nil
	}
	cli := filepath.Join(core, "cli.js")
	if info, err := os.Stat(cli); err != nil || !info.Mode().IsRegular() {
		return nil
	}
	return []string{m.cfg.Node, cli, "install", "--only-shell", "--no-remove", "chromium"}
}

// InstallID is the latest Chromium install command, "" when none ran.
func (m *Manager) InstallID() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.install
}

// SetInstallID records the Chromium install command that runs now.
func (m *Manager) SetInstallID(id string) {
	m.mu.Lock()
	m.install = id
	m.mu.Unlock()
}

// InstallEnv is the environment of the install command: the service's own,
// so Chromium lands where the service looks for it.
func (m *Manager) InstallEnv() []string {
	if m.cfg.Env != nil {
		return slices.Clone(m.cfg.Env)
	}
	return serviceEnv(os.Environ())
}

// playwrightCore finds the playwright-core package the service at script
// loads, the way Node resolves it: node_modules of its folder or any parent.
func playwrightCore(script string) string {
	if script == "" {
		return ""
	}
	dir := filepath.Dir(script)
	for range 8 {
		candidate := filepath.Join(dir, "node_modules", "playwright-core")
		if info, err := os.Stat(filepath.Join(candidate, "browsers.json")); err == nil && info.Mode().IsRegular() {
			return candidate
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			break
		}
		dir = parent
	}
	return ""
}

// chromiumInstalled reports whether the headless shell the service launches
// (the revision its playwright-core pins) is in Playwright's cache. known is
// false when that cannot be told; the service's own error then says it.
func chromiumInstalled(script string, env []string) (installed, known bool) {
	core := playwrightCore(script)
	if core == "" {
		return false, false
	}
	data, err := os.ReadFile(filepath.Join(core, "browsers.json"))
	if err != nil {
		return false, false
	}
	var manifest struct {
		Browsers []struct {
			Name     string `json:"name"`
			Revision string `json:"revision"`
		} `json:"browsers"`
	}
	if json.Unmarshal(data, &manifest) != nil {
		return false, false
	}
	revision := ""
	for _, b := range manifest.Browsers {
		if b.Name == "chromium-headless-shell" {
			revision = b.Revision
		}
	}
	cache := playwrightCache(core, env)
	if revision == "" || cache == "" {
		return false, false
	}
	_, err = os.Stat(filepath.Join(cache, "chromium_headless_shell-"+revision, "INSTALLATION_COMPLETE"))
	return err == nil, true
}

// playwrightCache is where Playwright keeps its browsers for env.
func playwrightCache(core string, env []string) string {
	lookup := func(name string) string {
		for _, kv := range env {
			if k, v, ok := strings.Cut(kv, "="); ok && k == name {
				return v
			}
		}
		return ""
	}
	if path := lookup("PLAYWRIGHT_BROWSERS_PATH"); path == "0" {
		return filepath.Join(core, ".local-browsers")
	} else if path != "" {
		return path
	}
	home := lookup("HOME")
	switch runtime.GOOS {
	case "darwin":
		if home == "" {
			return ""
		}
		return filepath.Join(home, "Library", "Caches", "ms-playwright")
	case "windows":
		if local := lookup("LOCALAPPDATA"); local != "" {
			return filepath.Join(local, "ms-playwright")
		}
		return ""
	}
	if cache := lookup("XDG_CACHE_HOME"); cache != "" {
		return filepath.Join(cache, "ms-playwright")
	}
	if home == "" {
		return ""
	}
	return filepath.Join(home, ".cache", "ms-playwright")
}
