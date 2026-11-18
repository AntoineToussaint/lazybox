package gitpublish

import (
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"
)

// ErrNoOrigin: the repository has no origin remote, so there is nothing to
// refresh; a workspace starts from what the repository has.
var ErrNoOrigin = errors.New("this repository has no origin remote")

// Refreshed is a branch of origin as fetched just now: its default branch
// (RefreshDefault) or one chosen to start from (RefreshBranch).
type Refreshed struct {
	Branch string // the branch's name on origin (main, trunk…)
	Ref    string // refs/remotes/origin/<Branch>, updated by the fetch
	Commit string // its commit
	// Default is origin's default branch as origin names it now; empty when
	// origin did not say.
	Default string
}

// RefreshDefault asks origin which branch its HEAD names, fetches that branch
// into refs/remotes/origin/<branch> and records refs/remotes/origin/HEAD, so a
// new workspace branch can start from origin's default branch as it is now
// rather than from whatever the source copy or checkout last saw.
//
// root is a source copy (bare) or the person's own checkout: only remote-
// tracking refs and origin/HEAD change; local branches, the index and the
// working tree are never touched. token authenticates an HTTPS GitHub origin
// exactly as Push does (an HTTP header in the child's environment only, the
// person's global config and hooks left out); empty uses the person's own Git
// setup (SSH keys, a credential helper, local remotes). ErrNoOrigin when there
// is no origin; any other error means origin could not be reached or read,
// and the caller falls back to the last known default branch.
func (g Git) RefreshDefault(ctx context.Context, root, token string) (Refreshed, error) {
	defer lock(root)()
	remote, err := g.remote(ctx, root, token)
	if err != nil {
		return Refreshed{}, err
	}
	// Origin's HEAD names its default branch; it is never assumed to be main.
	out, err := remote(30*time.Second, "ls-remote", "--symref", "origin", "HEAD")
	if err != nil {
		return Refreshed{}, err
	}
	branch := symref(out)
	if branch == "" || branch == "HEAD" || strings.HasPrefix(branch, "-") {
		return Refreshed{}, errors.New("origin does not name a default branch")
	}
	if _, err = read(ctx, root, "check-ref-format", "refs/heads/"+branch); err != nil {
		return Refreshed{}, errors.New("origin names an invalid default branch")
	}
	ref := "refs/remotes/origin/" + branch
	if _, err = remote(2*time.Minute, "fetch", "--no-tags", "--no-recurse-submodules", "origin", "+refs/heads/"+branch+":"+ref); err != nil {
		return Refreshed{}, err
	}
	if _, err = run(ctx, invocation{root: root, args: []string{"symbolic-ref", "-m", "lazybox: origin's default branch", "refs/remotes/origin/HEAD", ref}}); err != nil {
		return Refreshed{}, err
	}
	commit, err := read(ctx, root, "rev-parse", "--verify", ref+"^{commit}")
	if err != nil {
		return Refreshed{}, err
	}
	return Refreshed{Branch: branch, Ref: ref, Commit: commit, Default: branch}, nil
}

// ErrNoBranch: origin answered and has no branch of that name, so there is
// nothing to start from (unlike an unreachable origin, which falls back to
// the last fetch).
var ErrNoBranch = errors.New("origin has no such branch")

// RefreshBranch is RefreshDefault for a branch the person chose to start
// from: it fetches refs/heads/<branch> into refs/remotes/origin/<branch> and
// leaves origin/HEAD alone. Refreshed.Default is origin's default branch as
// origin names it now, so the caller can tell a choice of the default from
// another branch. ErrNoBranch when origin has no such branch; ErrNoOrigin
// without origin; any other error means origin could not be reached.
func (g Git) RefreshBranch(ctx context.Context, root, token, branch string) (Refreshed, error) {
	if err := ValidBranch(ctx, root, branch); err != nil {
		return Refreshed{}, err
	}
	defer lock(root)()
	remote, err := g.remote(ctx, root, token)
	if err != nil {
		return Refreshed{}, err
	}
	out, err := remote(30*time.Second, "ls-remote", "--symref", "origin", "HEAD", "refs/heads/"+branch)
	if err != nil {
		return Refreshed{}, err
	}
	found := false
	for _, line := range strings.Split(out, "\n") {
		if _, name, ok := strings.Cut(line, "\t"); ok && !strings.HasPrefix(line, "ref: ") && strings.TrimSpace(name) == "refs/heads/"+branch {
			found = true
		}
	}
	if !found {
		return Refreshed{}, ErrNoBranch
	}
	ref := "refs/remotes/origin/" + branch
	if _, err = remote(2*time.Minute, "fetch", "--no-tags", "--no-recurse-submodules", "origin", "+refs/heads/"+branch+":"+ref); err != nil {
		return Refreshed{}, err
	}
	commit, err := read(ctx, root, "rev-parse", "--verify", ref+"^{commit}")
	if err != nil {
		return Refreshed{}, err
	}
	return Refreshed{Branch: branch, Ref: ref, Commit: commit, Default: symref(out)}, nil
}

// RemoteBranch is one branch origin has.
type RemoteBranch struct {
	Name   string
	Commit string
}

// RemoteBranches asks origin for its branches and its default branch without
// fetching any of them (one ls-remote): what a "start from" picker lists.
// Same credentials and errors as RefreshDefault.
func (g Git) RemoteBranches(ctx context.Context, root, token string) ([]RemoteBranch, string, error) {
	remote, err := g.remote(ctx, root, token)
	if err != nil {
		return nil, "", err
	}
	out, err := remote(30*time.Second, "ls-remote", "--symref", "origin", "HEAD", "refs/heads/*")
	if err != nil {
		return nil, "", err
	}
	var branches []RemoteBranch
	for _, line := range strings.Split(out, "\n") {
		commit, name, ok := strings.Cut(line, "\t")
		if !ok || strings.HasPrefix(line, "ref: ") {
			continue
		}
		if branch, ok := strings.CutPrefix(strings.TrimSpace(name), "refs/heads/"); ok && branch != "" {
			branches = append(branches, RemoteBranch{Name: branch, Commit: strings.TrimSpace(commit)})
		}
	}
	return branches, symref(out), nil
}

// ValidBranch refuses what cannot be a branch name, or could be read as an option.
func ValidBranch(ctx context.Context, root, branch string) error {
	if branch == "" || branch == "HEAD" || strings.HasPrefix(branch, "-") || len(branch) > 255 {
		return fmt.Errorf("%q is not a branch name", branch)
	}
	if _, err := read(ctx, root, "check-ref-format", "refs/heads/"+branch); err != nil {
		return fmt.Errorf("%q is not a branch name", branch)
	}
	return nil
}

// symref is the branch an ls-remote --symref answer says origin's HEAD names.
func symref(out string) string {
	for _, line := range strings.Split(out, "\n") {
		target, name, ok := strings.Cut(strings.TrimPrefix(line, "ref: "), "\t")
		if ok && strings.HasPrefix(line, "ref: ") && strings.TrimSpace(name) == "HEAD" {
			return strings.TrimPrefix(target, "refs/heads/")
		}
	}
	return ""
}

// remote returns a runner for git commands that talk to origin, with the
// credentials RefreshDefault describes; ErrNoOrigin without origin.
func (g Git) remote(ctx context.Context, root, token string) (func(time.Duration, ...string) (string, error), error) {
	remote, err := read(ctx, root, "config", "--get", "remote.origin.url")
	if err != nil || remote == "" {
		return nil, ErrNoOrigin
	}
	origin, _ := g.Origin(ctx, root)
	env, prefix, secrets := environment(), []string{"-c", "core.hooksPath=" + os.DevNull}, []string(nil)
	if origin.HTTPS && token != "" {
		header := "Authorization: Basic " + base64.StdEncoding.EncodeToString([]byte("x-access-token:"+token))
		prefix = append(prefix, "-c", "credential.helper=", "-c", "http.followRedirects=false")
		env = append(env, "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull, "GIT_ASKPASS=", "SSH_ASKPASS=", "GIT_CONFIG_COUNT=1", "GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader", "GIT_CONFIG_VALUE_0="+header)
		secrets = []string{token, base64.StdEncoding.EncodeToString([]byte("x-access-token:" + token))}
	} else if os.Getenv("GIT_SSH_COMMAND") == "" && os.Getenv("GIT_SSH") == "" {
		env = append(env, "GIT_SSH_COMMAND=ssh -o BatchMode=yes")
	}
	return func(timeout time.Duration, args ...string) (string, error) {
		return run(ctx, invocation{root: root, timeout: timeout, env: env, secrets: secrets, args: append(append([]string(nil), prefix...), args...)})
	}, nil
}
