package codex

import (
	"os"
	"os/exec"
	"path/filepath"
)

// Codex locates native helpers next to its executable. Launch the package's
// entry point when PATH exposes it through a symlink without its companions.
func codexProgram(binary string) string {
	if binary == "" {
		binary = "codex"
	}
	path, err := exec.LookPath(binary)
	if err != nil {
		return binary
	}
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil || resolved == path {
		return path
	}
	helper := "codex-code-mode-host"
	if filepath.Ext(resolved) == ".exe" {
		helper += ".exe"
	}
	if !packagedHelper(path, helper) && packagedHelper(resolved, helper) {
		return resolved
	}
	return path
}

func packagedHelper(program, name string) bool {
	info, err := os.Stat(filepath.Join(filepath.Dir(program), name))
	return err == nil && info.Mode().IsRegular() && (info.Mode().Perm()&0111 != 0 || filepath.Ext(name) == ".exe")
}
