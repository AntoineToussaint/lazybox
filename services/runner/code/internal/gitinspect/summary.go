package gitinspect

import (
	"bytes"
	"context"
	"strconv"
	"strings"
)

// Summary is a repository's state at a glance, for a sidebar row: where the
// branch is against its pushed copy and the default branch, and how many
// files are uncommitted. It reads local refs only and never fetches, so
// "behind" is as of the last fetch or push.
type Summary struct {
	// Branch is empty on a detached HEAD.
	Branch, Head string
	// Upstream is origin/<branch> once pushed. Ahead and Behind count against
	// it; before the first push Ahead counts the commits since base.
	Upstream      string
	Ahead, Behind int
	// Default is origin/HEAD, else origin/main, origin/master, main or master.
	Default                         string
	AheadOfDefault, BehindOfDefault int
	Dirty                           int
	DirtyTruncated                  bool
}

// Summary costs four or five short Git processes and reads at most 1 MiB of
// status, whatever the size of the repository's history.
func (Git) Summary(ctx context.Context, root, base string) (Summary, error) {
	var out Summary
	raw, cut, err := run(ctx, root, "status", "--porcelain=v2", "--branch", "-z", "--untracked-files=normal", "--ignore-submodules=dirty")
	if err != nil {
		return out, err
	}
	out.DirtyTruncated = cut
	records := bytes.Split(raw, []byte{0})
	for i := 0; i < len(records); i++ {
		record := records[i]
		if len(record) == 0 {
			continue
		}
		if value, ok := bytes.CutPrefix(record, []byte("# branch.oid ")); ok {
			if head := string(value); head != "(initial)" {
				out.Head = head
			}
			continue
		}
		if value, ok := bytes.CutPrefix(record, []byte("# branch.head ")); ok {
			if branch := string(value); branch != "(detached)" {
				out.Branch = branch
			}
			continue
		}
		if record[0] == '#' {
			continue
		}
		// A rename or copy carries its original path as the next record.
		if record[0] == '2' {
			i++
		}
		if record[0] == '1' || record[0] == '2' || record[0] == 'u' || record[0] == '?' {
			out.Dirty++
		}
	}
	// Which of the candidate refs exist, and where origin/HEAD points, in one call.
	refs := []string{"refs/remotes/origin/HEAD", "refs/remotes/origin/main", "refs/remotes/origin/master", "refs/heads/main", "refs/heads/master"}
	if out.Branch != "" && validRef(out.Branch) {
		refs = append(refs, "refs/remotes/origin/"+out.Branch)
	}
	listed, _, err := run(ctx, root, append([]string{"for-each-ref", "--format=%(refname) %(symref)"}, refs...)...)
	if err != nil {
		return out, err
	}
	present := map[string]string{}
	for _, line := range strings.Split(strings.TrimSpace(string(listed)), "\n") {
		name, target, _ := strings.Cut(line, " ")
		if name != "" {
			present[name] = target
		}
	}
	if out.Branch != "" {
		if _, ok := present["refs/remotes/origin/"+out.Branch]; ok {
			out.Upstream = "origin/" + out.Branch
			out.Behind, out.Ahead = leftRight(ctx, root, "refs/remotes/origin/"+out.Branch)
		} else if base != "" && validRef(base) {
			_, out.Ahead = leftRight(ctx, root, base)
		}
	}
	if target := present["refs/remotes/origin/HEAD"]; strings.HasPrefix(target, "refs/remotes/") {
		out.Default = strings.TrimPrefix(target, "refs/remotes/")
	}
	if out.Default == "" {
		for _, candidate := range []string{"refs/remotes/origin/main", "refs/remotes/origin/master", "refs/heads/main", "refs/heads/master"} {
			if _, ok := present[candidate]; ok {
				out.Default = strings.TrimPrefix(strings.TrimPrefix(candidate, "refs/remotes/"), "refs/heads/")
				break
			}
		}
	}
	if out.Default != "" && out.Head != "" {
		out.BehindOfDefault, out.AheadOfDefault = leftRight(ctx, root, out.Default)
	}
	return out, nil
}

// leftRight counts commits only on ref (left) and only on HEAD (right). An
// unknown ref counts nothing: the summary is a hint, not a check.
func leftRight(ctx context.Context, root, ref string) (int, int) {
	raw, _, err := run(ctx, root, "rev-list", "--left-right", "--count", ref+"...HEAD", "--")
	if err != nil {
		return 0, 0
	}
	left, right, _ := strings.Cut(strings.TrimSpace(string(raw)), "\t")
	l, _ := strconv.Atoi(left)
	r, _ := strconv.Atoi(right)
	return l, r
}

// validRef refuses what Git would read as an option or a range.
func validRef(ref string) bool {
	return ref != "" && !strings.HasPrefix(ref, "-") && !strings.Contains(ref, "..") && !strings.ContainsAny(ref, " \t\n\x00")
}

// Changes is what a branch changes against where it forked from the default
// branch (else against the commit its workspace started from), uncommitted
// work included: what its pull request would show, as the sidebar's card counts it.
type Changes struct {
	// Base is the default branch compared with; empty when it is the workspace's start.
	Base                        string
	Files, Additions, Deletions int
	// Truncated: too many changes to list in full, so the counts are a floor.
	Truncated bool
}

// Changes costs three short Git processes (merge-base, diff --numstat and
// ls-files for untracked files), each bounded to 1 MiB of output. Untracked
// files count as files; their lines are not counted. It never fetches: the
// default branch is as of the last fetch or push.
func (Git) Changes(ctx context.Context, root, against, start string) (Changes, error) {
	var out Changes
	ref := ""
	if against != "" && validRef(against) {
		if raw, _, err := run(ctx, root, "merge-base", against, "HEAD"); err == nil {
			ref, out.Base = strings.TrimSpace(string(raw)), against
		}
	}
	if ref == "" && start != "" && validRef(start) {
		ref = start
	}
	if ref == "" {
		ref = "HEAD"
	}
	raw, cut, err := run(ctx, root, "diff", "--no-ext-diff", "--no-textconv", "--no-renames", "--ignore-submodules=dirty", "--numstat", "-z", ref, "--")
	if err != nil {
		return out, err
	}
	out.Truncated = cut
	for _, record := range bytes.Split(raw, []byte{0}) {
		added, rest, ok := bytes.Cut(record, []byte{'\t'})
		if !ok {
			continue
		}
		deleted, _, ok := bytes.Cut(rest, []byte{'\t'})
		if !ok {
			continue
		}
		out.Files++
		// A binary file reads "-\t-": a file changed, no lines counted.
		a, _ := strconv.Atoi(string(added))
		d, _ := strconv.Atoi(string(deleted))
		out.Additions += a
		out.Deletions += d
	}
	untracked, cut, err := run(ctx, root, "ls-files", "--others", "--exclude-standard", "-z")
	if err != nil {
		return out, err
	}
	out.Truncated = out.Truncated || cut
	out.Files += bytes.Count(untracked, []byte{0})
	return out, nil
}
