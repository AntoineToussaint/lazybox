package gitinspect

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// Head is the commit HEAD points at.
func (Git) Head(ctx context.Context, root string) (string, error) {
	out, _, err := run(ctx, root, "rev-parse", "--verify", "--quiet", "HEAD")
	if err != nil {
		return "", errors.New("this repository has no commit yet")
	}
	return strings.TrimSpace(string(out)), nil
}

// ShowAt reads a file's content at a commit. exists is false when the commit
// has no such file; content is bounded like every other read here.
func (Git) ShowAt(ctx context.Context, root, commit, path string) (content []byte, exists bool, err error) {
	if !filepath.IsLocal(path) || path == "." || strings.ContainsRune(path, 0) {
		return nil, false, errors.New("relative file path required")
	}
	if commit == "" || strings.HasPrefix(commit, "-") || strings.ContainsAny(commit, " :\x00") {
		return nil, false, errors.New("commit required")
	}
	if _, _, err = run(ctx, root, "cat-file", "-e", commit+":"+filepath.ToSlash(path)); err != nil {
		return nil, false, nil
	}
	out, cut, err := run(ctx, root, "show", "--no-textconv", commit+":"+filepath.ToSlash(path))
	if err != nil {
		return nil, false, err
	}
	if cut {
		return nil, true, errors.New("file is larger than 1 MiB")
	}
	return out, true, nil
}

// ReadWorking reads a file from the working tree without following a path out
// of it. exists is false when there is no such file.
func ReadWorking(root, path string) (content []byte, exists bool, err error) {
	if !filepath.IsLocal(path) || path == "." || strings.ContainsRune(path, 0) {
		return nil, false, errors.New("relative file path required")
	}
	file, err := os.OpenInRoot(root, path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	defer file.Close()
	stat, err := file.Stat()
	if err != nil {
		return nil, false, err
	}
	if !stat.Mode().IsRegular() {
		return nil, true, errors.New("not a regular file")
	}
	data, err := io.ReadAll(io.LimitReader(file, outputLimit+1))
	if err != nil {
		return nil, true, err
	}
	if len(data) > outputLimit {
		return nil, true, errors.New("file is larger than 1 MiB")
	}
	if bytes.IndexByte(data, 0) >= 0 {
		return nil, true, errors.New("binary file")
	}
	return data, true, nil
}
