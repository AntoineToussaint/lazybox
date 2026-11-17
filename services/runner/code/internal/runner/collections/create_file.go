package collections

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"
)

// ErrFileExists refuses CreateFile over a file already on disk: replacing one
// goes through SaveFile with the version the person saw.
var ErrFileExists = errors.New("a file already exists at this path")

// CreateFile writes a new text file inside one of the workspace's repository
// worktrees. The reference is "member/path"; missing folders are created. It
// never replaces a file, never follows a symlinked component, and never stages
// or commits anything.
func (m *Manager) CreateFile(ctx context.Context, id, reference, text string) (FilePreview, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if err := ctx.Err(); err != nil {
		return FilePreview{}, err
	}
	if len(text) > previewLimit || !utf8.ValidString(text) || strings.ContainsRune(text, 0) {
		return FilePreview{}, errors.New("a new file must be UTF-8 text of at most 1 MiB")
	}
	c, ok := m.items[id]
	if !ok || m.closed || c.State == "deleted" || c.State == "deleting" {
		return FilePreview{}, errors.New("workspace files are unavailable")
	}
	if !validRoot(m.root, c) {
		return FilePreview{}, errors.New("workspace root is invalid")
	}
	if resolved, err := filepath.EvalSymlinks(c.Root); err != nil || resolved != c.Root {
		return FilePreview{}, errors.New("workspace root moved or was replaced")
	}
	path := strings.TrimPrefix(strings.TrimSpace(reference), "./")
	if path == "" || len(path) > 4096 || filepath.IsAbs(path) || strings.ContainsRune(path, 0) {
		return FilePreview{}, errors.New("a workspace path (repository/path) is required")
	}
	path = filepath.Clean(filepath.FromSlash(path))
	if !filepath.IsLocal(path) {
		return FilePreview{}, errors.New("file is outside this workspace")
	}
	first, rest, _ := strings.Cut(path, string(filepath.Separator))
	member := false
	for _, candidate := range c.Members {
		folder, err := filepath.Rel(c.Root, candidate.Path)
		if err == nil && folder == first {
			member = true
			break
		}
	}
	if !member || rest == "" {
		return FilePreview{}, fmt.Errorf("choose a file inside one of this workspace's repositories, not %q", filepath.ToSlash(path))
	}
	if strings.Split(rest, string(filepath.Separator))[0] == ".git" {
		return FilePreview{}, errors.New("files cannot be written inside .git")
	}
	root, err := os.OpenRoot(c.Root)
	if err != nil {
		return FilePreview{}, err
	}
	defer root.Close()
	// Every existing component must be a real directory: a symlink would put
	// the file somewhere other than the path the person chose.
	parts := strings.Split(path, string(filepath.Separator))
	part := ""
	for i, component := range parts {
		part = filepath.Join(part, component)
		info, err := root.Lstat(part)
		if errors.Is(err, os.ErrNotExist) {
			break
		}
		if err != nil {
			return FilePreview{}, err
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return FilePreview{}, errors.New("the path goes through a symlink; choose the actual folder")
		}
		if i == len(parts)-1 {
			return FilePreview{}, fmt.Errorf("%w: %s", ErrFileExists, filepath.ToSlash(path))
		}
		if !info.IsDir() {
			return FilePreview{}, fmt.Errorf("%s is not a folder", filepath.ToSlash(part))
		}
	}
	if dir := filepath.Dir(path); dir != "." {
		if err = root.MkdirAll(dir, 0o755); err != nil {
			return FilePreview{}, err
		}
	}
	out, err := root.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o644)
	if errors.Is(err, os.ErrExist) {
		return FilePreview{}, fmt.Errorf("%w: %s", ErrFileExists, filepath.ToSlash(path))
	}
	if err != nil {
		return FilePreview{}, err
	}
	_, err = io.WriteString(out, text)
	if err == nil {
		err = out.Sync()
	}
	if err = errors.Join(err, out.Close()); err != nil {
		_ = root.Remove(path)
		return FilePreview{}, err
	}
	return m.readFileLocked(ctx, id, path)
}
