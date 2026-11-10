// Package gitinspect reads local Git changes without fetching, changing the index,
// invoking diff helpers/text conversion, or changing repository configuration.
package gitinspect

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
	"unicode/utf8"
)

const outputLimit = 1 << 20
const fileLimit = 2000

type File struct{ Path, OriginalPath, IndexStatus, WorktreeStatus string }
type Status struct {
	Branch, Head string
	Files        []File
	Truncated    bool
}
type Diff struct {
	Text              string
	Truncated, Binary bool
}

// Inspector is the read-only boundary used by workspace review transports.
type Inspector interface {
	Status(context.Context, string) (Status, error)
	Diff(context.Context, string, string, bool) (Diff, error)
}
type Git struct{}
type bounded struct {
	bytes.Buffer
	truncated bool
	limit     int
}

func (b *bounded) Write(p []byte) (int, error) {
	n := len(p)
	remaining := b.limit - b.Len()
	if len(p) > remaining {
		b.truncated = true
		p = p[:remaining]
	}
	_, _ = b.Buffer.Write(p)
	return n, nil
}
func run(ctx context.Context, root string, args ...string) ([]byte, bool, error) {
	ctx, cancel := context.WithTimeout(ctx, 8*time.Second)
	defer cancel()
	args = append([]string{"--no-optional-locks", "--literal-pathspecs", "-c", "core.fsmonitor=false", "-C", root}, args...)
	cmd := exec.CommandContext(ctx, "git", args...)
	out := &bounded{limit: outputLimit}
	stderr := &bounded{limit: 8192}
	cmd.Stdout = out
	cmd.Stderr = stderr
	err := cmd.Run()
	if ctx.Err() != nil {
		return nil, false, ctx.Err()
	}
	if err != nil {
		return nil, false, errors.New("Git inspection failed: " + stderr.String())
	}
	return out.Bytes(), out.truncated, nil
}
func (Git) Status(ctx context.Context, root string) (Status, error) {
	var result Status
	raw, cut, err := run(ctx, root, "status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none")
	if err != nil {
		return result, err
	}
	result.Truncated = cut
	parts := bytes.Split(raw, []byte{0})
	for i := 0; i < len(parts)-1; i++ {
		part := parts[i]
		if len(part) < 4 {
			return result, errors.New("invalid Git status record")
		}
		f := File{Path: string(part[3:]), IndexStatus: string(part[0]), WorktreeStatus: string(part[1])}
		if part[0] == 'R' || part[0] == 'C' || part[1] == 'R' || part[1] == 'C' {
			i++
			if i >= len(parts)-1 {
				result.Truncated = true
				break
			}
			f.OriginalPath = string(parts[i])
		}
		if len(result.Files) >= fileLimit {
			result.Truncated = true
			break
		}
		result.Files = append(result.Files, f)
	}
	head, _, err := run(ctx, root, "rev-parse", "--verify", "HEAD")
	if err != nil {
		return result, err
	}
	result.Head = strings.TrimSpace(string(head))
	branch, _, err := run(ctx, root, "rev-parse", "--abbrev-ref", "HEAD")
	if err != nil {
		return result, err
	}
	result.Branch = strings.TrimSpace(string(branch))
	return result, nil
}
func (g Git) Diff(ctx context.Context, root, path string, staged bool) (Diff, error) {
	if !filepath.IsLocal(path) || path == "." || strings.ContainsRune(path, 0) {
		return Diff{}, errors.New("relative changed-file path required")
	}
	status, err := g.Status(ctx, root)
	if err != nil {
		return Diff{}, err
	}
	var chosen *File
	for i := range status.Files {
		if status.Files[i].Path == path {
			chosen = &status.Files[i]
			break
		}
	}
	if chosen == nil {
		return Diff{}, errors.New("file is no longer in the current changes; refresh the list")
	}
	if chosen.IndexStatus == "?" {
		if staged {
			return Diff{}, errors.New("untracked files have no staged diff")
		}
		file, err := os.OpenInRoot(root, path)
		if err != nil {
			return Diff{}, err
		}
		defer file.Close()
		stat, err := file.Stat()
		if err != nil {
			return Diff{}, err
		}
		if !stat.Mode().IsRegular() {
			return Diff{}, errors.New("only regular file contents can be previewed")
		}
		data, err := io.ReadAll(io.LimitReader(file, outputLimit+1))
		if err != nil {
			return Diff{}, err
		}
		cut := len(data) > outputLimit
		if cut {
			data = data[:outputLimit]
		}
		if bytes.IndexByte(data, 0) >= 0 || !utf8.Valid(data) {
			return Diff{Binary: true, Truncated: cut}, nil
		}
		return Diff{Text: "New file: " + path + "\n\n" + string(data), Truncated: cut}, nil
	}
	args := []string{"diff", "--no-ext-diff", "--no-textconv", "--no-color", "--submodule=short"}
	if staged {
		args = append(args, "--cached")
	}
	args = append(args, "--", path)
	if staged && chosen.OriginalPath != "" {
		args = append(args, chosen.OriginalPath)
	}
	data, cut, err := run(ctx, root, args...)
	return Diff{Text: strings.ToValidUTF8(string(data), "�"), Truncated: cut, Binary: bytes.Contains(data, []byte("Binary files "))}, err
}
