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
		var add bytes.Buffer
		count := 0
		// Streamed: memory holds at most MaxUntracked paths however many
		// untracked files there are; the rest are only counted.
		err := call.stream(ctx, func(path string) error {
			if strings.HasSuffix(path, "/") {
				w.skip(path, "nested repository")
				return nil
			}
			if count >= limits.MaxUntracked {
				w.skip(path, "over the untracked file limit")
				return nil
			}
			info, err := os.Lstat(filepath.Join(top, filepath.FromSlash(path)))
			if err != nil {
				return nil // gone since it was listed
			}
			if info.Mode().IsRegular() && info.Size() > limits.MaxFileBytes {
				w.skip(path, fmt.Sprintf("%d MiB, over the %d MiB checkpoint limit", info.Size()>>20, limits.MaxFileBytes>>20))
				return nil
			}
			count++
			add.WriteString(path)
			add.WriteByte(0)
			return nil
		}, "ls-files", "-z", "--others", "--exclude-standard")
		if err != nil {
			return workTree{}, err
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

// rawDiff lists what differs from a to b, without rename detection. Restore
// needs every entry, so the list is not cut; it is parsed as Git writes it.
func rawDiff(ctx context.Context, top, a, b string) ([]entry, error) {
	entries := []entry{}
	var meta []string
	err := gitCall{dir: top}.stream(ctx, func(field string) error {
		if meta == nil {
			meta = strings.Fields(strings.TrimPrefix(field, ":"))
			if len(meta) != 5 {
				return errors.New("unexpected git diff-tree output")
			}
			return nil
		}
		entries = append(entries, entry{OldMode: meta[0], NewMode: meta[1], Status: meta[4][:1], Path: field})
		meta = nil
		return nil
	}, "diff-tree", "-r", "-z", "--no-renames", "--no-commit-id", a, b)
	if err != nil {
		return nil, err
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
	// Both listings are streamed and stop one file past changeLimit, so a
	// turn that touched a million files is read no further than shown.
	files := []FileChange{}
	byPath := map[string]int{}
	var pending []string // the current record's fields, status first
	err := gitCall{dir: top}.stream(ctx, func(field string) error {
		pending = append(pending, field)
		status := pending[0]
		if status == "" {
			return errStop
		}
		f := FileChange{Status: status[:1]}
		switch {
		case (f.Status == "R" || f.Status == "C") && len(pending) == 3:
			f.OriginalPath, f.Path = pending[1], pending[2]
		case f.Status != "R" && f.Status != "C" && len(pending) == 2:
			f.Path = pending[1]
		default:
			return nil
		}
		pending = pending[:0]
		byPath[f.Path] = len(files)
		files = append(files, f)
		if len(files) > changeLimit {
			return errStop
		}
		return nil
	}, "diff-tree", "-r", "-z", "-M", "--no-commit-id", "--name-status", a, b)
	if err != nil {
		return nil, false, err
	}
	truncated := len(files) > changeLimit
	if truncated {
		files = files[:changeLimit]
	}
	// numstat lists the same files in the same order: one record per file,
	// "added\tdeleted\tpath", or "added\tdeleted\t" then source and
	// destination for a rename.
	seen, renameFields := 0, 0
	var counts []string
	err = gitCall{dir: top}.stream(ctx, func(field string) error {
		path := ""
		if renameFields > 0 {
			renameFields--
			if renameFields > 0 {
				return nil // the rename's source
			}
			path = field
		} else {
			parts := strings.SplitN(field, "\t", 3)
			if len(parts) != 3 {
				return nil
			}
			counts = parts
			if parts[2] == "" {
				renameFields = 2
				return nil
			}
			path = parts[2]
		}
		seen++
		if index, ok := byPath[path]; ok && index < len(files) {
			if counts[0] == "-" {
				files[index].Binary = true
			} else {
				files[index].Additions, _ = strconv.Atoi(counts[0])
				files[index].Deletions, _ = strconv.Atoi(counts[1])
			}
		}
		if seen >= len(files) {
			return errStop
		}
		return nil
	}, "diff-tree", "-r", "-z", "-M", "--no-commit-id", "--numstat", a, b)
	if err != nil {
		return nil, false, err
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
// HEAD and branch are untouched. Files the from snapshot skipped (ignored,
// too large, past the untracked limit, nested repositories) are in neither
// tree, so a path that would overwrite or remove one is left as it is and
// reported. It returns what it changed and what it could not.
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
		full, err := inside(root, top, e.Path)
		if err != nil {
			problems = append(problems, e.Path+": "+err.Error())
			continue
		}
		if err = unsaved(top, full, e.Status); err != nil {
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

// errUnsaved names a file restore would overwrite that the safety checkpoint
// does not hold, so undoing the revert could not bring it back.
var errUnsaved = errors.New("left as it is: a file there is not in the saved checkpoint (ignored, too large, past the untracked limit, or in a nested repository)")

// unsaved refuses to write full when checkout-index -f would destroy bytes the
// safety tree (the from side of the diff) does not hold. Deletions have run, so
// what remains on disk at an added path, a directory with entries at any
// written path, or a non-directory where a directory must be created was
// skipped by the snapshot.
func unsaved(top, full, status string) error {
	for dir := filepath.Dir(full); dir != top && strings.HasPrefix(dir, top+string(filepath.Separator)); dir = filepath.Dir(dir) {
		info, err := os.Lstat(dir)
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return err
		}
		if !info.IsDir() {
			return errUnsaved
		}
	}
	info, err := os.Lstat(full)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	if info.IsDir() {
		entries, err := os.ReadDir(full)
		if err != nil {
			return err
		}
		if len(entries) > 0 {
			return errors.New("left as it is: a directory there still holds files the saved checkpoint does not (ignored or skipped)")
		}
		return nil
	}
	// Modified and type-changed paths are in the safety tree as they are on
	// disk; an added one is not.
	if status == "A" {
		return errUnsaved
	}
	return nil
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
