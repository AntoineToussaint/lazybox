package browser

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// sourceCheckout lays out a service the way a source checkout does:
// workers/browser/dist/main.js, with node_modules/playwright-core pinning a
// headless shell revision. It returns the script and Playwright's cache.
func sourceCheckout(t *testing.T) (script, cache string) {
	t.Helper()
	root := t.TempDir()
	script = filepath.Join(root, "workers", "browser", "dist", "main.js")
	core := filepath.Join(root, "node_modules", "playwright-core")
	cache = filepath.Join(root, "ms-playwright")
	for path, text := range map[string]string{
		script:                               "// the browser service\n",
		filepath.Join(core, "cli.js"):        "// playwright-core\n",
		filepath.Join(core, "browsers.json"): `{"browsers":[{"name":"chromium","revision":"1200"},{"name":"chromium-headless-shell","revision":"1248"}]}`,
	} {
		if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	return script, cache
}

func TestAvailabilitySaysWhyAgentsCannotBrowseAndHowToFixIt(t *testing.T) {
	script, cache := sourceCheckout(t)
	env := []string{"HOME=" + t.TempDir(), "PLAYWRIGHT_BROWSERS_PATH=" + cache}
	m := newManager(t, func(c *Config) { c.Script, c.Env, c.Node = script, env, "/usr/bin/node" })

	// Chromium missing: not installed, with the command that installs it.
	a := m.Availability()
	if a.State != AvailabilityNotInstalled || !strings.Contains(a.Problem, "isn’t installed") {
		t.Fatalf("missing Chromium: %+v", a)
	}
	want := []string{"/usr/bin/node", filepath.Join(filepath.Dir(filepath.Dir(filepath.Dir(filepath.Dir(script)))), "node_modules", "playwright-core", "cli.js"), "install", "--only-shell", "--no-remove", "chromium"}
	if !slices.Equal(a.Install, want) {
		t.Fatalf("install command %q, want %q", a.Install, want)
	}
	if !m.EnabledFor("ws-1") {
		t.Fatal("a missing Chromium must not withhold the tools: installing it makes them work without a restart")
	}
	if env := m.InstallEnv(); !slices.Contains(env, "PLAYWRIGHT_BROWSERS_PATH="+cache) {
		t.Fatalf("the install must put Chromium where the service looks: %q", env)
	}

	// Another revision installed does not count; the pinned one does.
	for _, dir := range []string{"chromium_headless_shell-1200", "chromium_headless_shell-1248"} {
		if err := os.MkdirAll(filepath.Join(cache, dir), 0o700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(cache, "chromium_headless_shell-1200", "INSTALLATION_COMPLETE"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if got := m.Availability().State; got != AvailabilityNotInstalled {
		t.Fatalf("an unfinished or other revision is not installed: %q", got)
	}
	if err := os.WriteFile(filepath.Join(cache, "chromium_headless_shell-1248", "INSTALLATION_COMPLETE"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if a := m.Availability(); a.State != AvailabilityReady || a.Problem != "" || a.Install != nil {
		t.Fatalf("installed: %+v", a)
	}

	// Off: the person's choice, said with how to turn it on.
	s := m.Settings()
	s.DefaultPermission = PermissionOff
	if _, err := m.SaveSettings(s); err != nil {
		t.Fatal(err)
	}
	if a := m.Availability(); a.State != AvailabilityOff || !strings.Contains(a.Problem, "Ask before each new site") {
		t.Fatalf("off: %+v", a)
	}

	// No service: unavailable, and why.
	none := newManager(t, func(c *Config) { c.Script = "" })
	if a := none.Availability(); a.State != AvailabilityUnavailable || !strings.Contains(a.Problem, "developer-build feature") {
		t.Fatalf("no service: %+v", a)
	}
}

func TestAvailabilityIsReadyWhenChromiumCannotBeChecked(t *testing.T) {
	// A service with no playwright-core beside it (a bundle): its own error
	// says whether Chromium is missing; the Runner does not guess.
	m := newManager(t)
	if a := m.Availability(); a.State != AvailabilityReady {
		t.Fatalf("unknown is not reported as missing: %+v", a)
	}
}

func TestPlaywrightCacheFollowsPlaywright(t *testing.T) {
	core := "/src/node_modules/playwright-core"
	if got := playwrightCache(core, []string{"PLAYWRIGHT_BROWSERS_PATH=0", "HOME=/h"}); got != filepath.Join(core, ".local-browsers") {
		t.Fatal(got)
	}
	if got := playwrightCache(core, []string{"PLAYWRIGHT_BROWSERS_PATH=/cache", "HOME=/h"}); got != "/cache" {
		t.Fatal(got)
	}
	if got := playwrightCache(core, []string{"HOME=/h"}); got == "" || !strings.HasPrefix(got, "/h/") || !strings.HasSuffix(got, "ms-playwright") {
		t.Fatal(got)
	}
}

func TestOffForAWorkspaceRefusesItsAgentsOnly(t *testing.T) {
	m := newManager(t)
	saved, err := m.SetWorkspaceOff("ws-1", true)
	if err != nil || !slices.Equal(saved.OffWorkspaces, []string{"ws-1"}) {
		t.Fatalf("%v %v", saved.OffWorkspaces, err)
	}
	if m.EnabledFor("ws-1") || !m.EnabledFor("ws-2") || !m.WorkspaceOff("ws-1") {
		t.Fatal("off for ws-1 only")
	}
	if _, err := call(t, m, alice, "browser_open", nil); err == nil || !strings.Contains(err.Error(), "off for this workspace") || !strings.Contains(err.Error(), "Give Claude the browser") {
		t.Fatalf("an agent whose workspace has the browser off is told why and how: %v", err)
	}
	// Saved through Settings → Browser like any change; kept sorted and once.
	s := m.Settings()
	s.OffWorkspaces = []string{"ws-3", "ws-1", "ws-3"}
	if got, err := m.SaveSettings(s); err != nil || !slices.Equal(got.OffWorkspaces, []string{"ws-1", "ws-3"}) {
		t.Fatalf("%v %v", got.OffWorkspaces, err)
	}
	s = m.Settings()
	s.OffWorkspaces = []string{"../escape"}
	if _, err := m.SaveSettings(s); err == nil {
		t.Fatal("an invalid workspace accepted")
	}
	if _, err := m.SetWorkspaceOff("ws-1", false); err != nil || m.WorkspaceOff("ws-1") {
		t.Fatalf("back on: %v", err)
	}
}
