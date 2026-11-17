package terminals

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
)

// Resolve confines a workspace-relative directory to the workspace root. The
// check runs on resolved paths, so a symbolic link inside the workspace that
// points outside it is refused.
func Resolve(root, directory string) (string, error) {
	if !filepath.IsAbs(root) {
		return "", errors.New("workspace root must be absolute")
	}
	base, err := filepath.EvalSymlinks(root)
	if err != nil {
		return "", errors.New("workspace root is unavailable")
	}
	directory = strings.TrimSpace(directory)
	if len(directory) > 4096 || strings.ContainsRune(directory, 0) {
		return "", errors.New("invalid terminal directory")
	}
	clean := filepath.Clean(directory)
	if directory == "" {
		clean = "."
	}
	if filepath.IsAbs(directory) || !filepath.IsLocal(clean) {
		return "", errors.New("terminal directory must be inside the workspace")
	}
	resolved, err := filepath.EvalSymlinks(filepath.Join(base, clean))
	if err != nil {
		return "", errors.New("terminal directory does not exist in the workspace")
	}
	rel, err := filepath.Rel(base, resolved)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) || filepath.IsAbs(rel) {
		return "", errors.New("terminal directory must be inside the workspace")
	}
	info, err := os.Stat(resolved)
	if err != nil || !info.IsDir() {
		return "", errors.New("terminal directory is not a directory")
	}
	return resolved, nil
}

// Shell picks the user's login shell: $SHELL when it names an executable,
// otherwise /bin/zsh, otherwise /bin/sh.
func Shell(preferred string) (string, error) {
	for _, path := range []string{preferred, "/bin/zsh", "/bin/sh"} {
		if !filepath.IsAbs(path) {
			continue
		}
		if info, err := os.Stat(path); err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0 {
			return path, nil
		}
	}
	return "", errors.New("no login shell found ($SHELL, /bin/zsh or /bin/sh)")
}

// Environment is the shell's environment: the user's own, without the Runner's
// configuration and service wiring, plus the terminal's identity and
// LAZYBOX_V2_COLLECTION_ID. The Runner adds the terminal's own credential
// (Spec.Prepare) and its CLI on PATH.
//
// Dropped: every inherited LAZYBOX_V2_* variable (Runner configuration such as
// its state directory), CODEFLY* (service wiring, including Codefly's scoped
// auth secret), the launching terminal's identity (TERM_PROGRAM and friends),
// PWD/OLDPWD/SHLVL, and CLAUDECODE (so an agent CLI started here is not
// mistaken for a nested one). Account credentials a managed profile would
// inject for an agent are never added.
// The owner's endpoint is never given: anything the person starts in the
// shell, such as an agent CLI, would inherit the owner's authority. An
// inherited LAZYBOX_V2_ENDPOINT is dropped with every other LAZYBOX_V2_*.
func Environment(inherited []string, collectionID string) []string {
	drop := map[string]bool{"TERM": true, "COLORTERM": true, "TERM_PROGRAM": true, "TERM_PROGRAM_VERSION": true, "TERM_SESSION_ID": true, "PWD": true, "OLDPWD": true, "SHLVL": true, "CLAUDECODE": true}
	env := make([]string, 0, len(inherited)+4)
	for _, entry := range inherited {
		name, _, ok := strings.Cut(entry, "=")
		if !ok || drop[name] || strings.HasPrefix(name, "LAZYBOX_V2_") || strings.HasPrefix(name, "CODEFLY") {
			continue
		}
		env = append(env, entry)
	}
	env = append(env, "TERM=xterm-256color", "COLORTERM=truecolor")
	if collectionID != "" {
		env = append(env, "LAZYBOX_V2_COLLECTION_ID="+collectionID)
	}
	return env
}
