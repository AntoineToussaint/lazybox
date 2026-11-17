package gitpublish

import (
	"context"
	"encoding/base64"
	"errors"
	"os"
	"strings"
	"time"
)

// ErrNoOrigin: the repository has no origin remote, so there is nothing to
// refresh; a workspace starts from what the repository has.
var ErrNoOrigin = errors.New("this repository has no origin remote")

// Refreshed is origin's default branch as fetched just now.
type Refreshed struct {
	Branch string // the default branch's name on origin (main, trunk…)
	Ref    string // refs/remotes/origin/<Branch>, updated by the fetch
	Commit string // its commit
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
	remote, err := read(ctx, root, "config", "--get", "remote.origin.url")
	if err != nil || remote == "" {
		return Refreshed{}, ErrNoOrigin
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
	remoteRun := func(timeout time.Duration, args ...string) (string, error) {
		return run(ctx, invocation{root: root, timeout: timeout, env: env, secrets: secrets, args: append(append([]string(nil), prefix...), args...)})
	}
	// Origin's HEAD names its default branch; it is never assumed to be main.
	out, err := remoteRun(30*time.Second, "ls-remote", "--symref", "origin", "HEAD")
	if err != nil {
		return Refreshed{}, err
	}
	branch := ""
	for _, line := range strings.Split(out, "\n") {
		target, name, ok := strings.Cut(strings.TrimPrefix(line, "ref: "), "\t")
		if ok && strings.HasPrefix(line, "ref: ") && strings.TrimSpace(name) == "HEAD" {
			branch = strings.TrimPrefix(target, "refs/heads/")
		}
	}
	if branch == "" || branch == "HEAD" || strings.HasPrefix(branch, "-") {
		return Refreshed{}, errors.New("origin does not name a default branch")
	}
	if _, err = read(ctx, root, "check-ref-format", "refs/heads/"+branch); err != nil {
		return Refreshed{}, errors.New("origin names an invalid default branch")
	}
	ref := "refs/remotes/origin/" + branch
	if _, err = remoteRun(2*time.Minute, "fetch", "--no-tags", "--no-recurse-submodules", "origin", "+refs/heads/"+branch+":"+ref); err != nil {
		return Refreshed{}, err
	}
	if _, err = run(ctx, invocation{root: root, args: []string{"symbolic-ref", "-m", "lazybox: origin's default branch", "refs/remotes/origin/HEAD", ref}}); err != nil {
		return Refreshed{}, err
	}
	commit, err := read(ctx, root, "rev-parse", "--verify", ref+"^{commit}")
	if err != nil {
		return Refreshed{}, err
	}
	return Refreshed{Branch: branch, Ref: ref, Commit: commit}, nil
}
