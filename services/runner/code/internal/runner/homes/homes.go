// Package homes owns home checkouts: the person's own, regular (non-worktree)
// clone of a repository, at a stable path such as ~/lazybox/repos/acme/api.
// It is where they run logins, long-lived dev servers, tool setup and manual
// tests of a module; agents only read it (WORKSPACES.md, "Home checkouts").
//
// Lazybox creates one on request, or adopts a folder the person already has,
// and never deletes, resets, commits in or switches the branch of one. Its
// branch is the person's. Workspaces stay worktrees of the Runner's source
// copy, so no workspace operation reaches a home.
//
// A created home is a `git clone --local` of the source copy: Git hard-links
// the immutable object files instead of copying them, so it is fast and costs
// little disk, yet it is a self-contained repository. It is deliberately not
// a worktree of the source (`git worktree add`): that would share the source's
// refs, config and branch checkout locks with every agent's workspace (an
// agent could move its branch, and its branch could not be checked out in a
// workspace), and it would break if the source copy were removed. Nor does it
// borrow objects through alternates (`clone --reference`), which a `git gc` in
// the source could corrupt. Its origin is the repository's real remote, read
// from the source copy, so fetch and push use the person's own credentials;
// Lazybox's GitHub token never enters it.
package homes

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
)

// Status is what a home holds now, read from local Git only (no fetch).
type Status struct {
	Exists   bool
	Branch   string
	Head     string
	Detached bool
	// Dirty counts changed and untracked files, at most MaxDirty.
	Dirty int
	Error string
	// DefaultBranch is the remote's default branch (origin/HEAD), and Behind
	// how many commits the checked-out branch lacks from origin/<it>, as last
	// fetched.
	DefaultBranch string
	Behind        int
}

// MaxDirty bounds how many changed files Inspect counts.
const MaxDirty = 1000

// git runs git in dir with the Runner's environment minus GIT_* (so nothing
// the Runner inherited redirects it to another repository), never prompting.
func git(ctx context.Context, dir string, args ...string) (string, error) {
	return gitInput(ctx, dir, "", args...)
}

func gitInput(ctx context.Context, dir, input string, args ...string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()
	cmd := exec.CommandContext(ctx, "git", append([]string{"-C", dir}, args...)...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = time.Second
	for _, e := range os.Environ() {
		if key, _, _ := strings.Cut(e, "="); strings.HasPrefix(key, "GIT_") {
			continue
		}
		cmd.Env = append(cmd.Env, e)
	}
	cmd.Env = append(cmd.Env, "GIT_TERMINAL_PROMPT=0")
	if input != "" {
		cmd.Stdin = strings.NewReader(input)
	}
	var out bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &out
	if err := cmd.Run(); err != nil {
		text := strings.TrimSpace(out.String())
		if len(text) > 400 {
			text = text[:400] + "…"
		}
		return "", fmt.Errorf("git %s: %w: %s", args[0], errors.Join(err, ctx.Err()), text)
	}
	return strings.TrimSpace(out.String()), nil
}

// Suggest is the default place for a repository's home: <root>/<owner>/<name>
// for a GitHub repository, else <root>/<name>.
func Suggest(root, sourceURL, name string) string {
	if root == "" {
		return ""
	}
	if full := gitidentity.GitHubName(sourceURL); full != "" {
		owner, repo, _ := strings.Cut(full, "/")
		return filepath.Join(root, owner, repo)
	}
	if name == "" {
		return ""
	}
	return filepath.Join(root, name)
}

// DefaultRoot is where default homes go when the Runner names none.
func DefaultRoot() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return ""
	}
	return filepath.Join(home, "lazybox", "repos")
}

// Inspect reads a home's branch, HEAD and changed-file count. It takes no
// lock and writes nothing (--no-optional-locks keeps `git status` from
// refreshing the person's index).
func Inspect(ctx context.Context, path string) Status {
	info, err := os.Stat(path)
	if err != nil || !info.IsDir() {
		return Status{Error: "The folder is missing"}
	}
	out, err := git(ctx, path, "--no-optional-locks", "status", "--porcelain=v2", "--branch", "--untracked-files=normal")
	if err != nil {
		return Status{Exists: true, Error: "Not a Git checkout"}
	}
	s := Status{Exists: true}
	scan := bufio.NewScanner(strings.NewReader(out))
	scan.Buffer(make([]byte, 64<<10), 1<<20)
	for scan.Scan() {
		line := scan.Text()
		switch {
		case strings.HasPrefix(line, "# branch.oid "):
			if oid := strings.TrimPrefix(line, "# branch.oid "); oid != "(initial)" && len(oid) >= 7 {
				s.Head = oid[:7]
			}
		case strings.HasPrefix(line, "# branch.head "):
			if head := strings.TrimPrefix(line, "# branch.head "); head == "(detached)" {
				s.Detached = true
			} else {
				s.Branch = head
			}
		case strings.HasPrefix(line, "#") || line == "":
		default:
			if s.Dirty < MaxDirty {
				s.Dirty++
			}
		}
	}
	s.DefaultBranch = defaultBranch(ctx, path)
	if s.DefaultBranch != "" && !s.Detached {
		if n, err := git(ctx, path, "rev-list", "--count", "HEAD..refs/remotes/origin/"+s.DefaultBranch); err == nil {
			fmt.Sscan(n, &s.Behind)
		}
	}
	return s
}

// defaultBranch is the remote's default branch: what origin/HEAD names, else
// main or master when origin has one. "" when it cannot be told offline.
func defaultBranch(ctx context.Context, path string) string {
	if ref, err := git(ctx, path, "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"); err == nil {
		if name, ok := strings.CutPrefix(ref, "refs/remotes/origin/"); ok && name != "" {
			return name
		}
	}
	for _, name := range []string{"main", "master"} {
		if _, err := git(ctx, path, "rev-parse", "--verify", "--quiet", "refs/remotes/origin/"+name); err == nil {
			return name
		}
	}
	return ""
}

// Update brings a home to the remote's latest default branch, safely: it
// fetches origin (with the person's own credentials; Git never prompts), then
// fast-forwards only when the default branch is checked out, nothing tracked
// is changed and the branch has no commits of its own. Otherwise it says why
// and leaves everything as it was: nothing is reset, stashed, discarded or
// switched. A fast-forward that would overwrite an untracked file is refused
// by Git itself.
func Update(ctx context.Context, path string) (updated bool, message string, err error) {
	before := Inspect(ctx, path)
	if !before.Exists || before.Error != "" {
		return false, "", fmt.Errorf("cannot update the home checkout: %s", strings.ToLower(before.Error))
	}
	if _, err = git(ctx, path, "fetch", "--quiet", "--prune", "origin"); err != nil {
		return false, "", fmt.Errorf("could not fetch origin (check your network and Git credentials for it): %w", err)
	}
	branch := defaultBranch(ctx, path)
	upstream := "origin/" + branch
	switch {
	case branch == "":
		return false, "Fetched origin. Not updated: its default branch is unknown (set it with git remote set-head origin --auto).", nil
	case before.Detached || before.Branch != branch:
		on := before.Branch
		if before.Detached {
			on = "a detached HEAD"
		}
		return false, fmt.Sprintf("Fetched origin. Not updated: the home is on %s, not %s; switch to %s yourself to update it.", on, branch, branch), nil
	}
	if changed, _ := git(ctx, path, "--no-optional-locks", "status", "--porcelain", "--untracked-files=no"); changed != "" {
		return false, fmt.Sprintf("Fetched origin. Not updated: %s has uncommitted changes; commit or stash them yourself first.", branch), nil
	}
	if ahead, _ := git(ctx, path, "rev-list", "--count", upstream+"..HEAD"); ahead != "" && ahead != "0" {
		return false, fmt.Sprintf("Fetched origin. Not updated: %s has %s commit(s) that are not on %s.", branch, ahead, upstream), nil
	}
	head, _ := git(ctx, path, "rev-parse", "--short", "HEAD")
	target, _ := git(ctx, path, "rev-parse", "--short", upstream)
	if head == target {
		return false, fmt.Sprintf("Already at the latest %s (%s).", upstream, head), nil
	}
	if _, err = git(ctx, path, "merge", "--ff-only", "--quiet", upstream); err != nil {
		return false, fmt.Sprintf("Fetched origin. Not updated: Git refused the fast-forward to %s (an untracked file would be overwritten?).", upstream), nil
	}
	return true, fmt.Sprintf("Updated %s to the latest %s (%s → %s).", branch, upstream, head, target), nil
}

// Identity is the checkout-independent identity of the repository a checkout
// belongs to (gitidentity.Repository), and its top-level folder.
func Identity(ctx context.Context, path string) (identity, top string, err error) {
	top, err = git(ctx, path, "rev-parse", "--show-toplevel")
	if err != nil {
		return "", "", errors.New("that folder is not a Git checkout")
	}
	if top, err = filepath.EvalSymlinks(top); err != nil {
		return "", "", err
	}
	common, err := git(ctx, path, "rev-parse", "--path-format=absolute", "--git-common-dir")
	if err != nil {
		return "", "", err
	}
	if common, err = filepath.EvalSymlinks(common); err != nil {
		return "", "", err
	}
	remote, _ := git(ctx, path, "config", "--get", "remote.origin.url")
	return gitidentity.Repository(common, remote), top, nil
}

// OriginFolder is the local folder a checkout's origin names (a clone of a
// repository on this computer), resolved, or "" when origin is a URL.
func OriginFolder(ctx context.Context, path string) string {
	remote, err := git(ctx, path, "config", "--get", "remote.origin.url")
	if err != nil || remote == "" {
		return ""
	}
	remote = strings.TrimPrefix(remote, "file://")
	if !filepath.IsAbs(remote) {
		return ""
	}
	resolved, err := filepath.EvalSymlinks(remote)
	if err != nil {
		return ""
	}
	return resolved
}

// Create clones source (the Runner's source copy, bare or not) into dest,
// which must not exist yet, as a self-contained checkout of the default
// branch whose origin is the source's own remote. It is built beside dest
// and renamed into place, so dest never holds a half-made clone.
func Create(ctx context.Context, source, dest string) error {
	if !filepath.IsAbs(source) || !filepath.IsAbs(dest) {
		return errors.New("absolute source and destination required")
	}
	if _, err := os.Lstat(dest); err == nil {
		return errors.New("that folder already exists: choose “Use an existing folder…” to adopt it, or another path")
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	parent := filepath.Dir(dest)
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return err
	}
	staging, err := os.MkdirTemp(parent, ".lazybox-home-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(staging)
	clone := filepath.Join(staging, "checkout")
	branch, _ := git(ctx, source, "symbolic-ref", "--short", "HEAD")
	remote, _ := git(ctx, source, "config", "--get", "remote.origin.url")
	if _, err = git(ctx, staging, "clone", "--local", "--no-checkout", "--", source, clone); err != nil {
		return err
	}
	// The clone's remote-tracking refs are the source's local branches,
	// agents' workspace branches among them. Replace them with the source's
	// own view of its remote, then point origin at that remote.
	if refs, _ := git(ctx, clone, "for-each-ref", "--format=delete %(refname)", "refs/remotes/origin/"); refs != "" {
		if _, err = gitInput(ctx, clone, refs+"\n", "update-ref", "--no-deref", "--stdin"); err != nil {
			return err
		}
	}
	if remote != "" {
		if _, err = git(ctx, clone, "fetch", "--quiet", "--no-tags", "--", source, "+refs/remotes/origin/*:refs/remotes/origin/*"); err != nil {
			return err
		}
		if _, err = git(ctx, clone, "remote", "set-url", "origin", remote); err != nil {
			return err
		}
	} else if branch != "" {
		// The source is the remote itself (a local repository without one).
		if _, err = git(ctx, clone, "fetch", "--quiet", "--no-tags", "--", source, "+refs/heads/"+branch+":refs/remotes/origin/"+branch); err != nil {
			return err
		}
	}
	start := "HEAD"
	if branch != "" {
		if _, err = git(ctx, clone, "rev-parse", "--verify", "--quiet", "refs/remotes/origin/"+branch); err == nil {
			start = "origin/" + branch
			_, _ = git(ctx, clone, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/"+branch)
		}
		_, err = git(ctx, clone, "checkout", "--quiet", "-B", branch, start)
	} else {
		_, err = git(ctx, clone, "checkout", "--quiet", "--detach", start)
	}
	if err != nil {
		return err
	}
	if err = os.Rename(clone, dest); err != nil {
		return err
	}
	return nil
}

// Editors are the editors Open knows, by id: the macOS application name and
// the command elsewhere. Nothing else is ever started.
var Editors = map[string]struct{ App, Command string }{
	"vscode":   {"Visual Studio Code", "code"},
	"cursor":   {"Cursor", "cursor"},
	"zed":      {"Zed", "zed"},
	"sublime":  {"Sublime Text", "subl"},
	"intellij": {"IntelliJ IDEA", "idea"},
}

// OpenCommand is the program and arguments that show path in app: "finder"
// (the file manager) or an editor id.
func OpenCommand(goos, path, app string) ([]string, error) {
	if app == "finder" {
		if goos == "darwin" {
			return []string{"open", path}, nil
		}
		return []string{"xdg-open", path}, nil
	}
	editor, ok := Editors[app]
	if !ok {
		return nil, fmt.Errorf("unknown app %q: finder or an editor (vscode, cursor, zed, sublime, intellij)", app)
	}
	if goos == "darwin" {
		return []string{"open", "-a", editor.App, path}, nil
	}
	return []string{editor.Command, path}, nil
}

// Open shows path in app on this computer.
func Open(ctx context.Context, path, app string) error {
	argv, err := OpenCommand(runtime.GOOS, path, app)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, argv[0], argv[1:]...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if out, err := cmd.CombinedOutput(); err != nil {
		text := strings.TrimSpace(string(out))
		if editor, ok := Editors[app]; ok {
			return fmt.Errorf("could not open %s: is it installed? %s", editor.App, text)
		}
		return fmt.Errorf("could not open the folder: %s", text)
	}
	return nil
}
