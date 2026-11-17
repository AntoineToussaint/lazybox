package codecheckpoints

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// Limits bound what checkpoints write and keep. Zero values select defaults.
type Limits struct {
	// MaxFileBytes skips an untracked file larger than this (a build output or
	// dataset nobody ignored); tracked files are always included.
	MaxFileBytes int64
	// MaxUntracked bounds how many untracked files one checkpoint includes.
	MaxUntracked int
	// KeepTurns is how many turns each conversation keeps checkpoints for.
	KeepTurns int
	// KeepReverts is how many reverts each conversation can still undo.
	KeepReverts int
}

func (l Limits) withDefaults() Limits {
	if l.MaxFileBytes <= 0 {
		l.MaxFileBytes = 32 << 20
	}
	if l.MaxUntracked <= 0 {
		l.MaxUntracked = 20000
	}
	if l.KeepTurns <= 0 {
		l.KeepTurns = 100
	}
	if l.KeepReverts <= 0 {
		l.KeepReverts = 20
	}
	return l
}

// workTree is one repository's files at one moment, as a Git tree.
type workTree struct {
	Top, Tree, Head, Branch string
	// Skipped names untracked paths left out (too large, too many, or a
	// nested repository); SkippedCount counts all of them.
	Skipped      []string
	SkippedCount int
}

const skippedListed = 20

func (w *workTree) skip(path, why string) {
	w.SkippedCount++
	if len(w.Skipped) < skippedListed {
		w.Skipped = append(w.Skipped, path+" ("+why+")")
	}
}

// repository resolves dir to its work tree root and its own index file.
func repository(ctx context.Context, dir string) (top, index string, err error) {
	out, err := git(ctx, dir, "rev-parse", "--show-toplevel", "--git-path", "index")
	if err != nil {
		return "", "", err
	}
	lines := strings.Split(out, "\n")
	if len(lines) != 2 || lines[0] == "" {
		return "", "", errors.New("not a Git work tree")
	}
	top, index = lines[0], lines[1]
	if !filepath.IsAbs(index) {
		index = filepath.Join(dir, index)
	}
	return top, index, nil
}

// snapshot writes the work tree's files, as Git would commit them after
// adding everything that is not ignored, as a tree object. It uses a private
// copy of the repository's index (for its file-status cache, so unchanged
// files are not hashed again), never the index itself.
func snapshot(ctx context.Context, dir, scratch string, limits Limits) (workTree, error) {
	top, realIndex, err := repository(ctx, dir)
	if err != nil {
		return workTree{}, err
	}
	w := workTree{Top: top}
	w.Head, _ = git(ctx, top, "rev-parse", "-q", "--verify", "HEAD^{commit}")
	w.Branch, _ = git(ctx, top, "symbolic-ref", "-q", "--short", "HEAD")
	tmp, err := os.MkdirTemp(scratch, "snapshot-")
	if err != nil {
		return workTree{}, err
	}
	defer os.RemoveAll(tmp)
	index := filepath.Join(tmp, "index")
	if err = copyFile(realIndex, index); err != nil && !errors.Is(err, os.ErrNotExist) {
		return workTree{}, err
	}
	call := gitCall{dir: top, index: index}
	// Tracked files: modifications and deletions, conflicts resolved as they are on disk.
	if _, _, err = call.run(ctx, "add", "-u"); err != nil {
		return workTree{}, err
	}
	for attempt := 0; ; attempt++ {
		w.Skipped, w.SkippedCount = nil, 0
		raw, _, err := call.run(ctx, "ls-files", "-z", "--others", "--exclude-standard")
		if err != nil {
			return workTree{}, err
		}
		var add bytes.Buffer
		count := 0
		for _, path := range splitNUL(raw) {
			if strings.HasSuffix(path, "/") {
				w.skip(path, "nested repository")
				continue
			}
			info, err := os.Lstat(filepath.Join(top, filepath.FromSlash(path)))
			if err != nil {
				continue // gone since it was listed
			}
			if info.Mode().IsRegular() && info.Size() > limits.MaxFileBytes {
				w.skip(path, fmt.Sprintf("%d MiB, over the %d MiB checkpoint limit", info.Size()>>20, limits.MaxFileBytes>>20))
				continue
			}
			if count >= limits.MaxUntracked {
				w.skip(path, "over the untracked file limit")
				continue
			}
			count++
			add.WriteString(path)
			add.WriteByte(0)
		}
		if count == 0 {
			break
		}
		// A file removed between listing and adding fails the add; list again once.
		if _, _, err = (gitCall{dir: top, index: index, stdin: add.Bytes()}).run(ctx, "add", "--pathspec-from-file=-", "--pathspec-file-nul"); err == nil {
			break
		} else if attempt > 0 {
			return workTree{}, err
		}
	}
	out, _, err := call.run(ctx, "write-tree")
	if err != nil {
		return workTree{}, err
	}
	w.Tree = strings.TrimSpace(string(out))
	return w, nil
}

func copyFile(from, to string) error {
	in, err := os.Open(from)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(to, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	if _, err = io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

// commit records tree as a commit whose parent is the HEAD it was taken on,
// so a checkpoint reads as "these files, on top of that commit".
func commit(ctx context.Context, top string, w workTree, message string) (string, error) {
	args := []string{"commit-tree", w.Tree, "-m", message}
	if w.Head != "" {
		args = append(args, "-p", w.Head)
	}
	return git(ctx, top, args...)
}

// entry is one path that differs between two trees.
type entry struct {
	Status           string
	Path             string
	OldMode, NewMode string
}

const gitlink = "160000"

// rawDiff lists what differs from a to b, without rename detection.
func rawDiff(ctx context.Context, top, a, b string) ([]entry, error) {
	out, _, err := gitCall{dir: top}.run(ctx, "diff-tree", "-r", "-z", "--no-renames", "--no-commit-id", a, b)
	if err != nil {
		return nil, err
	}
	fields := splitNUL(out)
	entries := []entry{}
	for i := 0; i+1 < len(fields); i += 2 {
		meta := strings.Fields(strings.TrimPrefix(fields[i], ":"))
		if len(meta) != 5 {
			return nil, errors.New("unexpected git diff-tree output")
		}
		entries = append(entries, entry{OldMode: meta[0], NewMode: meta[1], Status: meta[4][:1], Path: fields[i+1]})
	}
	return entries, nil
}

// FileChange is one file that differs between two checkpoints.
type FileChange struct {
	Path, OriginalPath string
	// Status is Git's: A added, M modified, D deleted, R renamed, T type changed.
	Status               string
	Additions, Deletions int
	Binary               bool
}

const changeLimit = 2000

// changes lists what differs from a to b, renames included, with line counts.
func changes(ctx context.Context, top, a, b string) ([]FileChange, bool, error) {
	if a == b {
		return []FileChange{}, false, nil
	}
	names, _, err := gitCall{dir: top}.run(ctx, "diff-tree", "-r", "-z", "-M", "--no-commit-id", "--name-status", a, b)
	if err != nil {
		return nil, false, err
	}
	files := []FileChange{}
	byPath := map[string]int{}
	fields := splitNUL(names)
	for i := 0; i < len(fields); {
		status := fields[i]
		if status == "" {
			break
		}
		f := FileChange{Status: status[:1]}
		if (f.Status == "R" || f.Status == "C") && i+2 < len(fields) {
			f.OriginalPath, f.Path = fields[i+1], fields[i+2]
			i += 3
		} else if i+1 < len(fields) {
			f.Path = fields[i+1]
			i += 2
		} else {
			break
		}
		byPath[f.Path] = len(files)
		files = append(files, f)
	}
	stats, _, err := gitCall{dir: top}.run(ctx, "diff-tree", "-r", "-z", "-M", "--no-commit-id", "--numstat", a, b)
	if err != nil {
		return nil, false, err
	}
	fields = splitNUL(stats)
	for i := 0; i < len(fields); i++ {
		parts := strings.SplitN(fields[i], "\t", 3)
		if len(parts) != 3 {
			continue
		}
		path := parts[2]
		if path == "" && i+2 < len(fields) {
			path = fields[i+2] // a rename: the source and destination follow
			i += 2
		}
		index, ok := byPath[path]
		if !ok {
			continue
		}
		if parts[0] == "-" {
			files[index].Binary = true
			continue
		}
		files[index].Additions, _ = strconv.Atoi(parts[0])
		files[index].Deletions, _ = strconv.Atoi(parts[1])
	}
	truncated := len(files) > changeLimit
	if truncated {
		files = files[:changeLimit]
	}
	return files, truncated, nil
}

// Diff is one file's change between two checkpoints, for a person to read.
type Diff struct {
	Text              string
	Truncated, Binary bool
}

func fileDiff(ctx context.Context, top, a, b, path, original string) (Diff, error) {
	args := []string{"diff", "--no-ext-diff", "--no-textconv", "--no-color", "-M", a, b, "--", path}
	if original != "" && original != path {
		args = append(args, original)
	}
	out, cut, err := gitCall{dir: top, limit: diffLimit}.run(ctx, args...)
	if err != nil {
		return Diff{}, err
	}
	return Diff{Text: strings.ToValidUTF8(string(out), "�"), Truncated: cut, Binary: bytes.Contains(out, []byte("Binary files "))}, nil
}

// restore makes the work tree's files match tree to, given they currently
// match tree from: it writes only the paths that differ, through a private
// index, and deletes the files to does not have. The repository's index,
// HEAD and branch are untouched; ignored files are never in either tree, so
// they stay as they are. It returns what it changed and what it could not.
func restore(ctx context.Context, top, scratch, from, to string) ([]entry, []string, error) {
	diff, err := rawDiff(ctx, top, from, to)
	if err != nil || len(diff) == 0 {
		return nil, nil, err
	}
	root, err := filepath.EvalSymlinks(top)
	if err != nil {
		return nil, nil, err
	}
	problems := []string{}
	var write bytes.Buffer
	changed := []entry{}
	// Deletions first: a directory a file replaces must be empty by then.
	for _, e := range diff {
		if e.OldMode == gitlink || e.NewMode == gitlink {
			problems = append(problems, e.Path+": nested repository left as it is")
			continue
		}
		if e.Status != "D" {
			continue
		}
		full, err := inside(root, top, e.Path)
		if err != nil {
			problems = append(problems, e.Path+": "+err.Error())
			continue
		}
		if err = os.Remove(full); err != nil && !errors.Is(err, os.ErrNotExist) {
			problems = append(problems, e.Path+": "+err.Error())
			continue
		}
		removeEmptyParents(top, filepath.Dir(full))
		changed = append(changed, e)
	}
	for _, e := range diff {
		if e.Status == "D" || e.OldMode == gitlink || e.NewMode == gitlink {
			continue
		}
		if _, err := inside(root, top, e.Path); err != nil {
			problems = append(problems, e.Path+": "+err.Error())
			continue
		}
		write.WriteString(e.Path)
		write.WriteByte(0)
		changed = append(changed, e)
	}
	if write.Len() > 0 {
		tmp, err := os.MkdirTemp(scratch, "restore-")
		if err != nil {
			return nil, nil, err
		}
		defer os.RemoveAll(tmp)
		index := filepath.Join(tmp, "index")
		if _, _, err = (gitCall{dir: top, index: index}).run(ctx, "read-tree", to); err != nil {
			return nil, nil, err
		}
		// checkout-index writes content, modes and symlinks as Git would,
		// with the repository's filters, from the private index only.
		if _, _, err = (gitCall{dir: top, index: index, stdin: write.Bytes()}).run(ctx, "checkout-index", "-f", "-z", "--stdin"); err != nil {
			problems = append(problems, err.Error())
		}
	}
	return changed, problems, nil
}

// inside resolves a repository-relative path, refusing one that escapes the
// work tree through ".." or a symbolic link in its directories.
func inside(root, top, path string) (string, error) {
	clean := filepath.Clean(filepath.FromSlash(path))
	if path == "" || filepath.IsAbs(clean) || clean == ".." || strings.HasPrefix(clean, ".."+string(filepath.Separator)) {
		return "", errors.New("path outside the repository")
	}
	full := filepath.Join(top, clean)
	// The nearest directory that exists decides: the rest is created under it.
	dir := filepath.Dir(full)
	for {
		if _, err := os.Lstat(dir); err == nil || dir == top || !strings.HasPrefix(dir, top) {
			break
		}
		dir = filepath.Dir(dir)
	}
	parent, err := filepath.EvalSymlinks(dir)
	if err != nil {
		return "", err
	}
	if rel, err := filepath.Rel(root, parent); err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return "", errors.New("path leads outside the repository")
	}
	return full, nil
}

func removeEmptyParents(top, dir string) {
	for dir != top && strings.HasPrefix(dir, top+string(filepath.Separator)) {
		if os.Remove(dir) != nil {
			return
		}
		dir = filepath.Dir(dir)
	}
}
