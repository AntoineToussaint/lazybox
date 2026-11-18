package runner

import (
	"errors"
	"os"
	"path/filepath"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
)

// OpenTerminal starts the user's login shell in a workspace: at its root, or in
// the workspace-relative directory given (a member repository, for example).
// Only a person reaches this, through the authenticated Runner API; agents have
// no terminal tool. The shell gets the user's environment, this Runner's CLI
// first on PATH and the terminal's own credential (terminal_credentials.go),
// so `lazybox log` and `lazybox run` publish to the workspace without flags.
func (b *Bench) OpenTerminal(collectionID, directory string, cols, rows uint16, requestKey string) (*terminals.Terminal, error) {
	b.terminalStart.Lock()
	defer b.terminalStart.Unlock()
	if b.Collections == nil {
		return nil, errors.New("workspaces unavailable")
	}
	c, err := b.Collections.Get(collectionID)
	if err != nil {
		return nil, err
	}
	if c.State == "deleting" || c.State == "deleted" {
		return nil, errors.New("workspace is being deleted")
	}
	cwd, err := terminals.Resolve(c.Root, directory)
	if err != nil {
		return nil, err
	}
	shell, err := terminals.Shell(os.Getenv("SHELL"))
	if err != nil {
		return nil, err
	}
	// Resolve confined cwd to the resolved root, so this is a local path.
	directory = ""
	if root, err := filepath.EvalSymlinks(c.Root); err == nil {
		if rel, err := filepath.Rel(root, cwd); err == nil && rel != "." {
			directory = rel
		}
	}
	title := c.Name
	if directory != "" {
		title += " / " + directory
	}
	env := b.withCLI(terminals.Environment(os.Environ(), collectionID))
	return b.Terminals.Start(terminals.Spec{CollectionID: collectionID, Directory: directory, Cwd: cwd, Title: title, Shell: shell, Env: env, Cols: cols, Rows: rows, Prepare: b.terminalCredential(collectionID)}, requestKey)
}
