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
	"slices"
	"strconv"
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
	Committed(context.Context, string, string) (Committed, error)
	CommittedDiff(context.Context, string, string, string) (Diff, error)
	Branches(context.Context, string) ([]string, string, error)
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

const commitLimit = 50

type Commit struct {
	ID, Subject string
	Time        int64
}

// Committed is what a branch's commits changed since base, the commit its
// workspace started from. Files carry the change in IndexStatus (A, M, D, R…).
type Committed struct {
	Files     []File
	Commits   []Commit
	Truncated bool
}

func verifyBase(ctx context.Context, root, base string) error {
	if base == "" || strings.HasPrefix(base, "-") || strings.ContainsAny(base, " \x00") {
		return errors.New("no base commit recorded for this repository")
	}
	_, _, err := run(ctx, root, "rev-parse", "--verify", "--quiet", base+"^{commit}")
	if err != nil {
		return errors.New("the workspace's base commit is no longer in this repository")
	}
	return nil
}

// Committed lists the files changed from the merge base of base and HEAD to
// HEAD, and the commits since base, newest first.
func (Git) Committed(ctx context.Context, root, base string) (Committed, error) {
	var result Committed
	if err := verifyBase(ctx, root, base); err != nil {
		return result, err
	}
	raw, cut, err := run(ctx, root, "diff", "--name-status", "-z", "-M", "--no-color", base+"...HEAD")
	if err != nil {
		return result, err
	}
	result.Truncated = cut
	parts := bytes.Split(raw, []byte{0})
	for i := 0; i < len(parts)-1; i++ {
		status := string(parts[i])
		if status == "" || i+1 >= len(parts)-1 {
			result.Truncated = result.Truncated || status != ""
			break
		}
		f := File{IndexStatus: status[:1], Path: string(parts[i+1])}
		i++
		if f.IndexStatus == "R" || f.IndexStatus == "C" {
			if i+1 >= len(parts)-1 {
				result.Truncated = true
				break
			}
			f.OriginalPath, f.Path = f.Path, string(parts[i+1])
			i++
		}
		if len(result.Files) >= fileLimit {
			result.Truncated = true
			break
		}
		result.Files = append(result.Files, f)
	}
	log, _, err := run(ctx, root, "log", "--no-color", "--format=%H%x00%ct%x00%s%x00", "-n", strconv.Itoa(commitLimit+1), base+"..HEAD")
	if err != nil {
		return result, err
	}
	fields := bytes.Split(log, []byte{0})
	for i := 0; i+2 < len(fields); i += 3 {
		id := strings.TrimSpace(string(fields[i]))
		if id == "" {
			break
		}
		if len(result.Commits) >= commitLimit {
			result.Truncated = true
			break
		}
		at, _ := strconv.ParseInt(string(fields[i+1]), 10, 64)
		result.Commits = append(result.Commits, Commit{ID: id, Time: at, Subject: strings.ToValidUTF8(string(fields[i+2]), "�")})
	}
	return result, nil
}

// CommittedDiff is one file's diff across the branch's commits since base.
func (g Git) CommittedDiff(ctx context.Context, root, base, path string) (Diff, error) {
	if !filepath.IsLocal(path) || path == "." || strings.ContainsRune(path, 0) {
		return Diff{}, errors.New("relative changed-file path required")
	}
	committed, err := g.Committed(ctx, root, base)
	if err != nil {
		return Diff{}, err
	}
	var chosen *File
	for i := range committed.Files {
		if committed.Files[i].Path == path {
			chosen = &committed.Files[i]
			break
		}
	}
	if chosen == nil {
		return Diff{}, errors.New("file is no longer in the branch changes; refresh the list")
	}
	args := []string{"diff", "--no-ext-diff", "--no-textconv", "--no-color", "--submodule=short", "-M", base + "...HEAD", "--", path}
	if chosen.OriginalPath != "" {
		args = append(args, chosen.OriginalPath)
	}
	data, cut, err := run(ctx, root, args...)
	return Diff{Text: strings.ToValidUTF8(string(data), "�"), Truncated: cut, Binary: bytes.Contains(data, []byte("Binary files "))}, err
}

const branchLimit = 200

// Branches lists local and remote-tracking branches, and the default branch:
// origin/HEAD when the clone recorded it, else main or master.
func (Git) Branches(ctx context.Context, root string) ([]string, string, error) {
	raw, _, err := run(ctx, root, "for-each-ref", "--format=%(refname)", "--sort=-committerdate", "--count="+strconv.Itoa(branchLimit), "refs/heads", "refs/remotes")
	if err != nil {
		return nil, "", err
	}
	branches := []string{}
	for _, ref := range strings.Split(strings.TrimSpace(string(raw)), "\n") {
		if name, ok := strings.CutPrefix(ref, "refs/heads/"); ok {
			branches = append(branches, name)
		} else if name, ok := strings.CutPrefix(ref, "refs/remotes/"); ok && !strings.HasSuffix(name, "/HEAD") {
			branches = append(branches, name)
		}
	}
	if head, _, err := run(ctx, root, "symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"); err == nil {
		if name := strings.TrimSpace(string(head)); name != "" {
			return branches, name, nil
		}
	}
	for _, name := range []string{"main", "master", "origin/main", "origin/master"} {
		if slices.Contains(branches, name) {
			return branches, name, nil
		}
	}
	return branches, "", nil
}
