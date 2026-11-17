package runner

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/process"
)

// OrchestrationProcess is the process a mission in a workspace would run,
// resolved from its layers (internal/process), with what the Runner found.
type OrchestrationProcess struct {
	Effective *process.Effective
	// Candidates are the workspace repositories that carry a process file.
	Candidates []ProcessCandidate
	// Notice says why no repository file applies although some exist.
	Notice string
	// UserFile is where the user's process file is read from.
	UserFile string
}

// ProcessCandidate is a workspace repository with a process file.
type ProcessCandidate struct {
	RepositoryID, Name, Path, Digest string
	file                             *process.File
	root                             *os.Root
}

// ErrNoProcessFile is returned when the chosen repository has no process file.
var ErrNoProcessFile = errors.New("this workspace repository has no " + process.RepositoryFile)

// OrchestrationProcess resolves the orchestration process for a workspace
// (or, with no workspace, the user's file over the presets). repository picks
// the workspace repository whose file applies when several have one. It
// starts nothing; the Runner provides no container isolation yet.
func (b *Bench) OrchestrationProcess(collectionID, repository string, tasks int) (OrchestrationProcess, error) {
	out := OrchestrationProcess{UserFile: b.Config.ProcessFile}
	var user *process.File
	if out.UserFile != "" {
		data, err := readBounded(nil, out.UserFile)
		switch {
		case errors.Is(err, fs.ErrNotExist):
		case err != nil:
			return out, fmt.Errorf("read your process file: %w", err)
		default:
			user = process.Parse(process.LayerUser, out.UserFile, data)
		}
	}
	options := process.Options{Tasks: tasks, Isolation: "none"}
	var chosen *ProcessCandidate
	if collectionID != "" {
		if b.Collections == nil {
			return out, errors.New("workspaces require a configured worktreeRoot")
		}
		c, err := b.Collections.Get(collectionID)
		if err != nil {
			return out, err
		}
		for _, m := range c.Members {
			candidate, err := readProcessFile(m.RepositoryID, m.Name, m.Path)
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}
			if err != nil {
				return out, fmt.Errorf("read %s's process file: %w", m.Name, err)
			}
			defer candidate.root.Close()
			out.Candidates = append(out.Candidates, *candidate)
		}
		// repository is a member name, or a repository id when only one
		// member checks that repository out.
		var names []string
		var byID []*ProcessCandidate
		for i := range out.Candidates {
			c := &out.Candidates[i]
			names = append(names, c.Name)
			if repository != "" && c.Name == repository {
				chosen = c
			}
			if repository != "" && c.RepositoryID == repository {
				byID = append(byID, c)
			}
		}
		if chosen == nil && len(byID) > 1 {
			return out, fmt.Errorf("several workspace repositories check out %s; name one of them", repository)
		}
		if chosen == nil && len(byID) == 1 {
			chosen = byID[0]
		}
		switch {
		case repository != "" && chosen == nil:
			return out, fmt.Errorf("%w: %s", ErrNoProcessFile, repository)
		case repository == "" && len(out.Candidates) == 1:
			chosen = &out.Candidates[0]
		case repository == "" && len(out.Candidates) > 1:
			out.Notice = fmt.Sprintf("Several repositories in this workspace have a process file (%s); choose the one that applies.", strings.Join(names, ", "))
		}
	}
	var repositoryFile *process.File
	if chosen != nil {
		repositoryFile = chosen.file
		options.Files = chosen.root.FS()
	}
	out.Effective = process.Resolve(user, repositoryFile, options)
	return out, nil
}

// readProcessFile reads a worktree's process file without following a link
// out of the worktree: the file is untrusted repository content.
func readProcessFile(id, name, worktree string) (*ProcessCandidate, error) {
	root, err := os.OpenRoot(worktree)
	if err != nil {
		return nil, err
	}
	data, err := readBounded(root, process.RepositoryFile)
	if err != nil {
		root.Close()
		return nil, err
	}
	source := name + "/" + process.RepositoryFile
	f := process.Parse(process.LayerRepository, source, data)
	return &ProcessCandidate{RepositoryID: id, Name: name, Path: filepath.Join(worktree, process.RepositoryFile), Digest: f.Digest, file: f, root: root}, nil
}

// readBounded reads at most one byte over the process file limit, so an
// oversized file is reported as such without being read whole.
func readBounded(root *os.Root, name string) ([]byte, error) {
	var f *os.File
	var err error
	if root != nil {
		f, err = root.Open(name)
	} else {
		f, err = os.Open(name)
	}
	if err != nil {
		return nil, err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() {
		return nil, fmt.Errorf("%s is not a regular file", name)
	}
	return io.ReadAll(io.LimitReader(f, process.MaxFileBytes+1))
}
