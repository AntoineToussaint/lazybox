package collections

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"unicode/utf8"
)

const previewLimit = 1 << 20

type DirectoryEntry struct {
	Name, Path string
	Directory  bool
}
type FilePreview struct {
	Directory           bool
	Entries             []DirectoryEntry
	Path, Text, Version string
	Truncated, Binary   bool
}

// ReadFile resolves references against this workspace. os.Root confines symlinks
// and traversal during the actual open, not just an earlier string check.
func (m *Manager) ReadFile(ctx context.Context, id, reference string) (FilePreview, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.readFileLocked(ctx, id, reference)
}

func (m *Manager) readFileLocked(ctx context.Context, id, reference string) (FilePreview, error) {
	if err := ctx.Err(); err != nil {
		return FilePreview{}, err
	}
	c, ok := m.items[id]
	if !ok || m.closed || c.State == "deleted" || c.State == "deleting" {
		return FilePreview{}, errors.New("workspace files are unavailable")
	}
	if !validRoot(m.root, c) {
		return FilePreview{}, errors.New("workspace root is invalid")
	}
	resolved, err := filepath.EvalSymlinks(c.Root)
	if err != nil || resolved != c.Root {
		return FilePreview{}, errors.New("workspace root moved or was replaced")
	}
	path := strings.TrimSpace(reference)
	if path == "" || len(path) > 4096 || strings.ContainsRune(path, 0) {
		return FilePreview{}, errors.New("valid file reference required")
	}
	absolute := filepath.IsAbs(path)
	if absolute {
		path, err = filepath.Rel(c.Root, path)
		if err != nil {
			return FilePreview{}, err
		}
	}
	path = filepath.Clean(path)
	if !filepath.IsLocal(path) {
		return FilePreview{}, errors.New("file is outside this workspace")
	}
	root, err := os.OpenRoot(c.Root)
	if err != nil {
		return FilePreview{}, err
	}
	defer root.Close()
	// A repository-relative link is unambiguous only with one repository.
	candidates := []string{path}
	// Links may include this workspace's immutable folder name. Resolve that
	// exact prefix only; do not guess by stripping arbitrary path components.
	prefix := filepath.Base(c.Root) + string(filepath.Separator)
	if !absolute && strings.HasPrefix(path, prefix) {
		candidates = append(candidates, strings.TrimPrefix(path, prefix))
	}
	if !absolute && len(c.Members) == 1 && !strings.HasPrefix(path, c.Members[0].Name+string(filepath.Separator)) {
		candidates = append(candidates, filepath.Join(c.Members[0].Name, path))
	}
	var file *os.File
	for _, candidate := range candidates {
		file, err = root.OpenFile(candidate, os.O_RDONLY|syscall.O_NONBLOCK, 0)
		if err == nil {
			path = candidate
			break
		}
		if !errors.Is(err, os.ErrNotExist) {
			return FilePreview{}, errors.New("file cannot be opened inside this workspace")
		}
	}
	if err != nil {
		return FilePreview{}, errors.New("file not found; use a workspace-relative path including the repository name")
	}
	defer file.Close()
	stat, err := file.Stat()
	if err != nil {
		return FilePreview{}, err
	}
	if stat.IsDir() {
		entries, err := file.ReadDir(501)
		if err != nil && !errors.Is(err, io.EOF) {
			return FilePreview{}, err
		}
		result := FilePreview{Path: filepath.ToSlash(path), Directory: true, Truncated: len(entries) > 500}
		if len(entries) > 500 {
			entries = entries[:500]
		}
		for _, entry := range entries {
			if entry.Name() == ".git" {
				continue
			}
			// Do not present links as regular files or directories; opening remains
			// confined by os.Root even if an entry changes after this listing.
			if entry.Type()&os.ModeSymlink != 0 {
				continue
			}
			if !entry.IsDir() && !entry.Type().IsRegular() {
				continue
			}
			result.Entries = append(result.Entries, DirectoryEntry{Name: entry.Name(), Path: filepath.ToSlash(filepath.Join(path, entry.Name())), Directory: entry.IsDir()})
		}
		sort.Slice(result.Entries, func(i, j int) bool {
			a, b := result.Entries[i], result.Entries[j]
			if a.Directory != b.Directory {
				return a.Directory
			}
			return a.Name < b.Name
		})
		return result, nil
	}
	if !stat.Mode().IsRegular() {
		return FilePreview{}, errors.New("only regular files can be previewed")
	}
	data, err := io.ReadAll(io.LimitReader(file, previewLimit+1))
	if err != nil {
		return FilePreview{}, err
	}
	result := FilePreview{Path: filepath.ToSlash(path), Truncated: len(data) > previewLimit}
	if result.Truncated {
		data = data[:previewLimit]
		for len(data) > 0 && !utf8.Valid(data) && len(data) > previewLimit-4 {
			data = data[:len(data)-1]
		}
	}
	result.Binary = !utf8.Valid(data) || strings.ContainsRune(string(data), 0)
	if !result.Binary {
		result.Text = string(data)
		if !result.Truncated {
			result.Version = fileVersion(data, stat)
		}
	}
	return result, nil
}

var ErrFileChanged = errors.New("file changed on the Runner; reload to review the changes before saving")

func fileVersion(data []byte, stat os.FileInfo) string {
	hash := sha256.New()
	fmt.Fprintf(hash, "%s:%d:", stat.Mode(), stat.ModTime().UnixNano())
	hash.Write(data)
	return fmt.Sprintf("%x", hash.Sum(nil))
}

// SaveFile serializes Lazybox edits and compares the observed disk revision before
// atomic replacement. Existing mode bits are retained. External writers do not
// participate in this lock, so the revision is checked again immediately before rename.
func (m *Manager) SaveFile(ctx context.Context, id, reference, text, expected string) (FilePreview, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if expected == "" || len(text) > previewLimit || !utf8.ValidString(text) || strings.ContainsRune(text, 0) {
		return FilePreview{}, errors.New("saving requires a complete UTF-8 text file and its loaded version")
	}
	before, err := m.readFileLocked(ctx, id, reference)
	if err != nil {
		return FilePreview{}, err
	}
	if before.Directory || before.Truncated || before.Binary {
		return FilePreview{}, errors.New("directories, binary and truncated files cannot be edited")
	}
	// Retrying a save after a lost acknowledgement does not replace identical bytes again.
	if before.Text == text {
		return before, nil
	}
	if before.Version != expected {
		return FilePreview{}, ErrFileChanged
	}
	c := m.items[id]
	root, err := os.OpenRoot(c.Root)
	if err != nil {
		return FilePreview{}, err
	}
	defer root.Close()
	path := filepath.FromSlash(before.Path)
	// Editing through symlinks changes link semantics under atomic replacement.
	// Keep previews usable, but require an explicit regular path for editing.
	part := ""
	for _, component := range strings.Split(path, string(filepath.Separator)) {
		part = filepath.Join(part, component)
		info, err := root.Lstat(part)
		if err != nil {
			return FilePreview{}, err
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return FilePreview{}, errors.New("open the actual workspace file to edit; symlink editing is not supported")
		}
	}
	file, err := root.OpenFile(path, os.O_RDWR|syscall.O_NONBLOCK, 0)
	if err != nil {
		return FilePreview{}, err
	}
	info, err := file.Stat()
	file.Close()
	if err != nil {
		return FilePreview{}, err
	}
	if !info.Mode().IsRegular() || info.Mode().Perm()&0222 == 0 {
		return FilePreview{}, os.ErrPermission
	}
	temporary := filepath.Join(filepath.Dir(path), ".lazybox-save-"+rand.Text())
	out, err := root.OpenFile(temporary, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return FilePreview{}, err
	}
	defer root.Remove(temporary)
	if err = out.Chmod(info.Mode().Perm()); err == nil {
		_, err = io.WriteString(out, text)
	}
	if err == nil {
		err = out.Sync()
	}
	err = errors.Join(err, out.Close())
	if err != nil {
		return FilePreview{}, err
	}
	latest, err := m.readFileLocked(ctx, id, path)
	if err != nil {
		return FilePreview{}, err
	}
	if latest.Version != expected {
		return FilePreview{}, ErrFileChanged
	}
	if err = ctx.Err(); err != nil {
		return FilePreview{}, err
	}
	if err = root.Rename(temporary, path); err != nil {
		return FilePreview{}, err
	}
	return m.readFileLocked(ctx, id, path)
}
