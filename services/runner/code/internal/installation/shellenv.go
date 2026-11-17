package installation

import (
	"bytes"
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// launchd starts a job with PATH=/usr/bin:/bin:/usr/sbin:/sbin and no LANG.
// Agents need the person's tools (git, gh, codex, their package managers),
// so the service records the PATH and locale their login shell sets up,
// read once at install time, plus the usual Homebrew and user directories in
// case the shell could not be read. `lazybox service install` reads it again;
// `lazybox doctor` shows it.
var fallbackPath = []string{"/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"}

const marker = "__LAZYBOX_ENV__"

// ShellEnvironment is what the person's login shell exports: PATH and LANG.
type ShellEnvironment struct {
	Path, Lang string
	// Shell is the shell that was read, and Problem why it could not be.
	Shell, Problem string
}

// ReadShellEnvironment runs the login shell once, interactively (zsh reads
// .zshrc only then, which is where most people extend PATH), with no input
// and a deadline, and reads the values between markers so a noisy prompt or
// banner cannot corrupt them.
func ReadShellEnvironment(ctx context.Context, getenv func(string) string) ShellEnvironment {
	shell := getenv("SHELL")
	if shell == "" || !filepath.IsAbs(shell) {
		shell = "/bin/zsh"
	}
	env := ShellEnvironment{Shell: shell}
	ctx, cancel := context.WithTimeout(ctx, 8*time.Second)
	defer cancel()
	script := `printf '` + marker + `%s` + marker + `%s` + marker + `' "$PATH" "${LANG:-}"`
	cmd := exec.CommandContext(ctx, shell, "-l", "-i", "-c", script)
	cmd.Stdin = nil
	cmd.Env = append(os.Environ(), "TERM=dumb")
	out, err := cmd.Output()
	parts := bytes.Split(out, []byte(marker))
	if len(parts) < 4 {
		env.Problem = "could not read the login shell's PATH"
		if err != nil {
			env.Problem += ": " + err.Error()
		}
		return env
	}
	env.Path, env.Lang = string(parts[1]), string(parts[2])
	return env
}

// ServicePath is the PATH the Runner service runs with: the login shell's,
// then the fallback directories it does not already name, then the person's
// own bin directories. Relative and empty entries are dropped: a service has
// no meaningful working directory to resolve them against.
func ServicePath(shellPath, home string) string {
	seen := map[string]bool{}
	var out []string
	add := func(dir string) {
		dir = strings.TrimSpace(dir)
		if dir == "" || !filepath.IsAbs(dir) || seen[dir] {
			return
		}
		seen[dir] = true
		out = append(out, dir)
	}
	for _, dir := range strings.Split(shellPath, ":") {
		add(dir)
	}
	for _, dir := range fallbackPath {
		add(dir)
	}
	if home != "" {
		add(filepath.Join(home, ".local", "bin"))
	}
	return strings.Join(out, ":")
}
