// Package installation is where an installed Lazybox release keeps things:
// the runtime it ships inside Lazybox.app, and the per-person directories the
// Runner service, the `lazybox` CLI and the desktop app agree on
// (INSTALL.md). Dev and Working never use it: their launchers pass every
// path explicitly (scripts/runner-command.mjs).
//
// Nothing here picks a value a running system resolves: ports are bound by
// the Runner, the endpoint is the file the Runner writes, the version is the
// one the build stamped (internal/version).
package installation

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"golang.org/x/sys/unix"
)

// Layout is the runtime a release ships in Lazybox.app/Contents/Resources/runtime:
//
//	runtime/VERSION            the release version, one line
//	runtime/bin/lazybox        the CLI (agents find it through LAZYBOX_V2_CLI_DIR)
//	runtime/bin/lazybox-runner the Runner
//	runtime/node/bin/node      the pinned Node that runs the Claude worker
//	runtime/worker/dist/main.js and its production node_modules
//	runtime/web                the phone app the device gateway serves
//
// Node has a directory of its own so that putting the CLI first on an
// agent's PATH never puts this Node before the person's own.
type Layout struct {
	Root string
}

func (l Layout) Version() string    { return filepath.Join(l.Root, "VERSION") }
func (l Layout) Bin() string        { return filepath.Join(l.Root, "bin") }
func (l Layout) CLI() string        { return filepath.Join(l.Root, "bin", "lazybox") }
func (l Layout) Runner() string     { return filepath.Join(l.Root, "bin", "lazybox-runner") }
func (l Layout) Node() string       { return filepath.Join(l.Root, "node", "bin", "node") }
func (l Layout) Worker() string     { return filepath.Join(l.Root, "worker", "dist", "main.js") }
func (l Layout) WorkerRoot() string { return filepath.Join(l.Root, "worker") }
func (l Layout) Web() string        { return filepath.Join(l.Root, "web") }

// Docs is the documentation (docs, Markdown only) the help assistant reads.
func (l Layout) Docs() string { return filepath.Join(l.Root, "docs") }

// ClaudeBinary is the Claude Code executable the pinned SDK ships for this
// platform, the one the worker runs when no other is configured. The Linux
// bundle does not ship it (REMOTE_RUNNERS.md §5.3): there the person installs
// Claude Code, and this path does not exist.
func (l Layout) ClaudeBinary() string {
	arch := map[string]string{"amd64": "x64", "arm64": "arm64"}[runtime.GOARCH]
	return filepath.Join(l.Root, "worker", "node_modules", "@anthropic-ai", "claude-agent-sdk-"+runtime.GOOS+"-"+arch, "claude")
}

// ReadVersion is the release version the runtime was built as.
func (l Layout) ReadVersion() (string, error) {
	data, err := os.ReadFile(l.Version())
	if err != nil {
		return "", err
	}
	v := strings.TrimSpace(string(data))
	if v == "" || strings.ContainsAny(v, " \t\n/") {
		return "", fmt.Errorf("%s is not a version", l.Version())
	}
	return v, nil
}

// Check reports the first part of the runtime that is missing or not
// executable, so a broken install says what is wrong rather than failing later.
func (l Layout) Check() error {
	for _, exe := range []string{l.CLI(), l.Runner(), l.Node()} {
		info, err := os.Stat(exe)
		if err != nil {
			return fmt.Errorf("the Lazybox runtime is incomplete: %w", err)
		}
		if !info.Mode().IsRegular() || info.Mode()&0o111 == 0 {
			return fmt.Errorf("the Lazybox runtime is incomplete: %s is not executable", exe)
		}
	}
	if _, err := os.Stat(l.Worker()); err != nil {
		return fmt.Errorf("the Lazybox runtime is incomplete: %w", err)
	}
	if _, err := l.ReadVersion(); err != nil {
		return fmt.Errorf("the Lazybox runtime is incomplete: %w", err)
	}
	return nil
}

// ErrNotInstalled is returned for a binary that does not run from a release
// runtime (a Dev or Working build, `go run`, a test binary).
var ErrNotInstalled = errors.New("this lazybox is not part of an installed Lazybox release")

// FromExecutable finds the runtime a binary in runtime/bin belongs to.
// Symbolic links are followed, so the CLI linked into ~/.local/bin still
// finds the release it came from.
func FromExecutable(executable string) (Layout, error) {
	resolved, err := filepath.EvalSymlinks(executable)
	if err != nil {
		return Layout{}, err
	}
	bin := filepath.Dir(resolved)
	if filepath.Base(bin) != "bin" {
		return Layout{}, ErrNotInstalled
	}
	l := Layout{Root: filepath.Dir(bin)}
	if _, err := os.Stat(l.Version()); err != nil {
		return Layout{}, ErrNotInstalled
	}
	return l, nil
}

// Current is the runtime this process runs from.
func Current() (Layout, error) {
	executable, err := os.Executable()
	if err != nil {
		return Layout{}, err
	}
	return FromExecutable(executable)
}

// AppBundle is the Lazybox.app a runtime lives in, or "" when it is not in one.
func (l Layout) AppBundle() string {
	// <App>.app/Contents/Resources/runtime
	resources := filepath.Dir(l.Root)
	contents := filepath.Dir(resources)
	app := filepath.Dir(contents)
	if filepath.Base(resources) == "Resources" && filepath.Base(contents) == "Contents" && strings.HasSuffix(app, ".app") {
		return app
	}
	return ""
}

// Unstable explains why a runtime cannot back a background service: macOS
// runs an app opened straight from a disk image or the Downloads quarantine
// from a temporary, read-only copy (App Translocation), and a disk image goes
// away when it is ejected. A launchd job pointing there would break on the
// next login. Empty means the location is fine.
func (l Layout) Unstable() string {
	switch {
	case strings.Contains(l.Root, "/AppTranslocation/"):
		return "Lazybox is running from a temporary copy macOS made because it was opened from Downloads or a disk image. Move Lazybox to the Applications folder, then open it from there."
	case readOnly(l.Root):
		return readOnlyAdvice
	}
	return ""
}

// readOnly reports a path on a read-only volume, as a mounted disk image is.
var readOnly = func(path string) bool {
	var fs unix.Statfs_t
	if unix.Statfs(path, &fs) != nil {
		return false
	}
	return statfsReadOnly(&fs)
}
