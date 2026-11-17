package workspaceruntime

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path"
	"strings"
	"syscall"
)

// MaxCopiedFile bounds one copied file: .env files and local configuration,
// not data sets.
const MaxCopiedFile = 16 << 20

// CopyResult is what CopyFiles did.
type CopyResult struct {
	// Copied files, relative to the repository.
	Copied []string `json:"copied,omitempty"`
	// Patterns that matched nothing in the source checkout.
	Missing []string `json:"missing,omitempty"`
	// Kept: the worktree already had the file; it is never overwritten.
	Kept []string `json:"kept,omitempty"`
}

// CopyFiles copies the listed files (exact paths, or patterns like
// config/*.local.json) from the source checkout into a new worktree: the
// gitignored files a fresh checkout lacks, like .env. Only regular files are
// copied, never through a symbolic link, never out of either tree, never over
// a file the worktree already has. Each tree is opened as an os.Root, so a
// pattern cannot reach outside it.
func CopyFiles(source, worktree string, patterns []string) (CopyResult, error) {
	var out CopyResult
	if len(patterns) == 0 {
		return out, nil
	}
	from, err := os.OpenRoot(source)
	if err != nil {
		return out, fmt.Errorf("source checkout unavailable: %w", err)
	}
	defer from.Close()
	to, err := os.OpenRoot(worktree)
	if err != nil {
		return out, fmt.Errorf("worktree unavailable: %w", err)
	}
	defer to.Close()
	var failures []error
	for _, pattern := range patterns {
		matches := []string{pattern}
		if strings.ContainsAny(pattern, "*?[") {
			if matches, err = fs.Glob(from.FS(), pattern); err != nil {
				failures = append(failures, fmt.Errorf("%s: %w", pattern, err))
				continue
			}
		}
		found := false
		for _, name := range matches {
			info, err := from.Lstat(name)
			if errors.Is(err, os.ErrNotExist) {
				continue
			}
			if err != nil {
				failures = append(failures, fmt.Errorf("%s: %w", name, err))
				continue
			}
			found = true
			if !info.Mode().IsRegular() {
				if !info.IsDir() {
					failures = append(failures, fmt.Errorf("%s is not a regular file; it was not copied", name))
				}
				continue
			}
			if info.Size() > MaxCopiedFile {
				failures = append(failures, fmt.Errorf("%s is larger than %d MiB; it was not copied", name, MaxCopiedFile>>20))
				continue
			}
			if _, err := to.Lstat(name); err == nil {
				out.Kept = append(out.Kept, name)
				continue
			}
			if err := copyOne(from, to, name, info.Mode().Perm()); err != nil {
				failures = append(failures, fmt.Errorf("%s: %w", name, err))
				continue
			}
			out.Copied = append(out.Copied, name)
		}
		if !found {
			out.Missing = append(out.Missing, pattern)
		}
	}
	return out, errors.Join(failures...)
}

func copyOne(from, to *os.Root, name string, perm fs.FileMode) error {
	if dir := path.Dir(name); dir != "." {
		if err := to.MkdirAll(dir, 0o755); err != nil {
			return err
		}
	}
	src, err := from.OpenFile(name, os.O_RDONLY|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return err
	}
	defer src.Close()
	if info, err := src.Stat(); err != nil || !info.Mode().IsRegular() {
		return errors.New("not a regular file")
	}
	dst, err := to.OpenFile(name, os.O_WRONLY|os.O_CREATE|os.O_EXCL, perm)
	if err != nil {
		return err
	}
	_, err = io.Copy(dst, io.LimitReader(src, MaxCopiedFile+1))
	if closeErr := dst.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		_ = to.Remove(name)
	}
	return err
}
