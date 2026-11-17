// Package gitpublish is the Runner's only writer of a workspace member's Git
// history: it commits the member's worktree and pushes its branch to origin.
// Every command runs in that one worktree, never force-pushes, and never
// writes a credential to disk, argv, Git config or an error.
package gitpublish

import (
	"bytes"
	"context"
	"encoding/base64"
	"errors"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/redact"
)

// ErrNothingToCommit refuses an empty commit.
var ErrNothingToCommit = errors.New("nothing to commit: the selected files have no changes")

// ErrDetached refuses to commit or push without a branch.
var ErrDetached = errors.New("HEAD is detached; check out a branch first")

const messageLimit = 64 << 10

// Origin is where a member pushes. GitHub is owner/name when origin is a
// GitHub repository; HTTPS is true when pushing needs the GitHub connection's
// credential rather than the person's own SSH setup.
type Origin struct {
	GitHub string
	HTTPS  bool
}

// Tracking is the branch's push state.
type Tracking struct {
	Branch   string
	Upstream string // origin/<branch>, empty when never pushed
	Ahead    int
	// Published: the branch was pushed, as far as the repository knows: it
	// has a remote-tracking ref, or an upstream on origin (`git push -u`
	// records one). From a source whose origin has no fetch refspec (one
	// imported before repositories/fetchspec.go, a checkout set up so), an
	// agent's own push records only the upstream; Upstream stays empty.
	Published bool
}

type Committed struct {
	Commit, Subject, Branch string
	Files                   int
}

type Pushed struct {
	Branch, Upstream, Head string
}

// Git runs the real git binary.
type Git struct{}

// One write at a time per worktree: a double click must not commit twice.
var locks sync.Map

func lock(root string) func() {
	value, _ := locks.LoadOrStore(root, &sync.Mutex{})
	m := value.(*sync.Mutex)
	m.Lock()
	return m.Unlock
}

// environment is the Runner's environment without Git and GitHub variables
// that could redirect the command (GIT_DIR, GIT_INDEX_FILE, GH_TOKEN…),
// except the person's SSH command and where their config lives. Prompts are
// disabled: nobody can answer.
func environment() []string {
	var env []string
	for _, entry := range os.Environ() {
		key, _, _ := strings.Cut(entry, "=")
		switch key {
		case "GIT_SSH_COMMAND", "GIT_SSH", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_NOSYSTEM":
			env = append(env, entry)
			continue
		}
		if strings.HasPrefix(key, "GIT_") || strings.HasPrefix(key, "GH_") || strings.HasPrefix(key, "GITHUB_") {
			continue
		}
		env = append(env, entry)
	}
	return append(env, "GIT_TERMINAL_PROMPT=0", "GIT_EDITOR=true", "GCM_INTERACTIVE=never")
}

type invocation struct {
	root    string
	args    []string
	env     []string
	stdin   string
	timeout time.Duration
	secrets []string
}

// run returns trimmed stdout, or an error carrying the redacted stderr tail.
func run(ctx context.Context, in invocation) (string, error) {
	timeout := in.timeout
	if timeout == 0 {
		timeout = 30 * time.Second
	}
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, "git", append([]string{"--literal-pathspecs", "-c", "core.fsmonitor=false", "-C", in.root}, in.args...)...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = time.Second
	cmd.Env = in.env
	if cmd.Env == nil {
		cmd.Env = environment()
	}
	if in.stdin != "" {
		cmd.Stdin = strings.NewReader(in.stdin)
	}
	var stdout bytes.Buffer
	stderr := &redact.Tail{}
	secrets := []string{}
	for _, secret := range in.secrets {
		secrets = append(secrets, "SECRET="+secret)
	}
	stderr.Redact(secrets)
	cmd.Stdout, cmd.Stderr = &stdout, stderr
	err := cmd.Run()
	if ctx.Err() == context.DeadlineExceeded {
		return "", errors.New("git " + in.args[0] + " timed out")
	}
	if err != nil {
		return "", &Failure{Command: in.args[0], Output: stderr.Text() + tail(stdout.String())}
	}
	return strings.TrimSpace(stdout.String()), nil
}

func tail(text string) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	if len(text) > 2000 {
		text = text[len(text)-2000:]
	}
	return "\n" + text
}

// Failure is a git command that exited unsuccessfully; Output is its
// redacted diagnostic tail (hook output, a rejected push's reason).
type Failure struct{ Command, Output string }

func (f *Failure) Error() string {
	if f.Output == "" {
		return "git " + f.Command + " failed"
	}
	return "git " + f.Command + " failed: " + f.Output
}

func read(ctx context.Context, root string, args ...string) (string, error) {
	return run(ctx, invocation{root: root, args: append([]string{"--no-optional-locks"}, args...)})
}

// Branch is the checked-out branch, or ErrDetached.
func (Git) Branch(ctx context.Context, root string) (string, error) {
	branch, err := read(ctx, root, "symbolic-ref", "--quiet", "--short", "HEAD")
	if err != nil || branch == "" {
		return "", ErrDetached
	}
	return branch, nil
}

// DefaultBranch is origin's default branch as the clone recorded it
// (refs/remotes/origin/HEAD), without contacting origin; ErrNoDefault when
// the clone has none.
func (Git) DefaultBranch(ctx context.Context, root string) (string, error) {
	ref, err := read(ctx, root, "symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD")
	branch, ok := strings.CutPrefix(ref, "origin/")
	if err != nil || !ok || branch == "" {
		return "", ErrNoDefault
	}
	return branch, nil
}

// Contains reports whether commit is on the local branch (an ancestor of its
// tip, or the tip), without contacting origin. A commit the repository does
// not have is not on it.
func (Git) Contains(ctx context.Context, root, branch, commit string) bool {
	if !validCommit(commit) || branch == "" || strings.HasPrefix(branch, "-") {
		return false
	}
	_, err := read(ctx, root, "merge-base", "--is-ancestor", commit, "refs/heads/"+branch)
	return err == nil
}

func validCommit(commit string) bool {
	if len(commit) != 40 && len(commit) != 64 {
		return false
	}
	for _, c := range commit {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

// ErrNoDefault: the clone does not record origin's default branch.
var ErrNoDefault = errors.New("origin's default branch is not recorded in this clone")

// Head is the commit checked out.
func (Git) Head(ctx context.Context, root string) (string, error) {
	return read(ctx, root, "rev-parse", "--verify", "HEAD")
}

// Origin reads origin's URL without contacting it.
func (Git) Origin(ctx context.Context, root string) (Origin, error) {
	remote, err := read(ctx, root, "config", "--get", "remote.origin.url")
	if err != nil || remote == "" {
		return Origin{}, errors.New("this repository has no origin remote to push to")
	}
	origin := Origin{GitHub: gitidentity.GitHubName(remote)}
	if u, err := url.Parse(remote); err == nil && u.Scheme == "https" && origin.GitHub != "" {
		origin.HTTPS = true
	}
	return origin, nil
}

// Track reports where the branch was last pushed and how many commits are not
// there yet. A never-pushed branch counts its commits since base.
func (g Git) Track(ctx context.Context, root, base string) (Tracking, error) {
	branch, err := g.Branch(ctx, root)
	if err != nil {
		return Tracking{}, err
	}
	out := Tracking{Branch: branch}
	since := base
	if _, err := read(ctx, root, "rev-parse", "--verify", "--quiet", "refs/remotes/origin/"+branch); err == nil {
		out.Upstream = "origin/" + branch
		since = "refs/remotes/origin/" + branch
	}
	out.Published = out.Upstream != ""
	if !out.Published {
		remote, _ := read(ctx, root, "config", "--get", "branch."+branch+".remote")
		merge, _ := read(ctx, root, "config", "--get", "branch."+branch+".merge")
		out.Published = remote == "origin" && strings.HasPrefix(merge, "refs/heads/")
	}
	if since == "" || strings.HasPrefix(since, "-") {
		return out, nil
	}
	count, err := read(ctx, root, "rev-list", "--count", since+"..HEAD")
	if err != nil {
		return out, nil
	}
	out.Ahead, _ = strconv.Atoi(count)
	return out, nil
}

func validPath(path string) bool {
	return path != "" && path != "." && filepath.IsLocal(path) && !strings.ContainsRune(path, 0)
}

// Commit stages paths (every change when empty, untracked files included) and
// commits them with message. Changes outside paths, staged or not, stay as
// they were. Only this worktree's index and branch change.
func (g Git) Commit(ctx context.Context, root, message string, paths []string) (Committed, error) {
	message = strings.TrimSpace(message)
	if message == "" {
		return Committed{}, errors.New("write a commit message")
	}
	if len(message) > messageLimit {
		return Committed{}, errors.New("commit message is too long")
	}
	for _, path := range paths {
		if !validPath(path) {
			return Committed{}, errors.New("commit paths must be files inside the repository")
		}
	}
	defer lock(root)()
	branch, err := g.Branch(ctx, root)
	if err != nil {
		return Committed{}, err
	}
	scope := append([]string{"--"}, paths...)
	if len(paths) == 0 {
		scope = []string{"--", "."}
	}
	if _, err = run(ctx, invocation{root: root, args: append([]string{"add", "--all"}, scope...)}); err != nil {
		return Committed{}, friendly(err)
	}
	// diff --quiet exits 1 when there are staged changes in scope.
	if _, err = run(ctx, invocation{root: root, args: append([]string{"diff", "--cached", "--quiet", "--no-ext-diff"}, scope...)}); err == nil {
		return Committed{}, ErrNothingToCommit
	}
	args := []string{"commit", "--cleanup=strip", "--file=-"}
	if len(paths) > 0 {
		args = append(append(args, "--only"), scope...)
	}
	if _, err = run(ctx, invocation{root: root, args: args, stdin: message + "\n", timeout: 2 * time.Minute}); err != nil {
		return Committed{}, friendly(err)
	}
	out := Committed{Branch: branch}
	if out.Commit, err = read(ctx, root, "rev-parse", "HEAD"); err != nil {
		return out, err
	}
	out.Subject, _ = read(ctx, root, "log", "-1", "--format=%s")
	if files, err := read(ctx, root, "diff-tree", "--root", "--no-commit-id", "--name-only", "-r", "-z", "HEAD"); err == nil {
		out.Files = strings.Count(files, "\x00")
		if files != "" && !strings.HasSuffix(files, "\x00") {
			out.Files++
		}
	}
	return out, nil
}

// friendly turns the failures a person can fix into instructions.
func friendly(err error) error {
	var failure *Failure
	if !errors.As(err, &failure) {
		return err
	}
	text := failure.Output
	switch {
	case strings.Contains(text, "Please tell me who you are") || strings.Contains(text, "Author identity unknown") || strings.Contains(text, "empty ident name"):
		return errors.New("Git does not know who you are: set user.name and user.email (git config --global user.name \"Your Name\"), then commit again")
	case strings.Contains(text, "index.lock"):
		return errors.New("another Git command is using this repository (index.lock exists); wait for it, or for the agent, to finish and try again")
	case failure.Command == "commit":
		return errors.New("the commit was refused (a hook or signing failed):" + tail(text))
	case strings.Contains(text, "non-fast-forward") || strings.Contains(text, "[rejected]") || strings.Contains(text, "fetch first"):
		return errors.New("origin has commits this branch does not; update the branch (pull or rebase) before pushing. Lazybox never force-pushes")
	case strings.Contains(text, "protected branch") || strings.Contains(text, "GH006"):
		return errors.New("origin protects this branch; push to a feature branch and open a pull request:" + tail(text))
	case strings.Contains(text, "Authentication failed") || strings.Contains(text, "could not read Username") || strings.Contains(text, "Permission denied") || strings.Contains(text, "403"):
		return errors.New("origin refused the credentials; check that the GitHub connection (or your SSH key) can write to this repository:" + tail(text))
	}
	return err
}

// Push pushes the checked-out branch to origin under the same name and
// records origin/<branch>. token authenticates an HTTPS GitHub origin and
// travels only in the child's environment as an HTTP header; empty uses the
// person's own Git setup (SSH keys, local remotes).
func (g Git) Push(ctx context.Context, root, token string) (Pushed, error) {
	defer lock(root)()
	branch, err := g.Branch(ctx, root)
	if err != nil {
		return Pushed{}, err
	}
	head, err := read(ctx, root, "rev-parse", "HEAD")
	if err != nil {
		return Pushed{}, err
	}
	origin, err := g.Origin(ctx, root)
	if err != nil {
		return Pushed{}, err
	}
	in := invocation{root: root, timeout: 5 * time.Minute, args: []string{"push", "--porcelain", "--set-upstream", "origin", "HEAD:refs/heads/" + branch}}
	if origin.HTTPS {
		if token == "" {
			return Pushed{}, errors.New("connect GitHub in Settings → Repositories to push to " + origin.GitHub)
		}
		header := "Authorization: Basic " + base64.StdEncoding.EncodeToString([]byte("x-access-token:"+token))
		// The person's global and system config are left out so a credential
		// helper or URL rewrite cannot capture or redirect the credential. No
		// hook runs: a pre-push hook the repository (or an agent) installed
		// would inherit the environment, and the credential with it.
		in.args = append([]string{"-c", "credential.helper=", "-c", "http.followRedirects=false", "-c", "core.hooksPath=" + os.DevNull}, in.args...)
		in.env = append(environment(), "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull, "GIT_ASKPASS=", "SSH_ASKPASS=", "GIT_CONFIG_COUNT=1", "GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader", "GIT_CONFIG_VALUE_0="+header)
		in.secrets = []string{token, base64.StdEncoding.EncodeToString([]byte("x-access-token:" + token))}
	} else {
		in.env = environment()
		if os.Getenv("GIT_SSH_COMMAND") == "" && os.Getenv("GIT_SSH") == "" {
			in.env = append(in.env, "GIT_SSH_COMMAND=ssh -o BatchMode=yes")
		}
	}
	if _, err = run(ctx, in); err != nil {
		return Pushed{}, friendly(err)
	}
	// Git records the remote-tracking ref only when origin's fetch refspec
	// covers the branch; record it whatever the refspec, so the push state
	// survives without a fetch.
	if _, err = run(ctx, invocation{root: root, args: []string{"update-ref", "-m", "lazybox: push", "refs/remotes/origin/" + branch, head}}); err != nil {
		return Pushed{}, err
	}
	return Pushed{Branch: branch, Upstream: "origin/" + branch, Head: head}, nil
}
