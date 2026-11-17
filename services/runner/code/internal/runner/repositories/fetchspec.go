package repositories

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

// A source copy is a bare clone, and `git clone --bare` gives its origin no
// fetch refspec. Without one, git maps nothing on origin to a remote-tracking
// ref: an agent's `git push -u` from a workspace worktree records the upstream
// but no refs/remotes/origin/<branch>, and `git fetch` updates no origin/*.
// The sidebar's git summary, ahead/behind and the PR loop then take a pushed
// branch for an unpushed one. A source copy's origin gets the refspec a normal
// clone has. Setting it fetches nothing: git uses it on the next push or fetch.

// OriginFetch is the refspec a non-bare clone gives origin.
const OriginFetch = "+refs/heads/*:refs/remotes/origin/*"

// EnsureOriginFetch gives the repository at gitDir's origin OriginFetch when
// origin has no fetch refspec at all. A repository without origin, or whose
// origin already has any refspec, is left exactly as it is. It reads and
// writes only the repository's own config, and never contacts origin.
func EnsureOriginFetch(ctx context.Context, gitDir string) (added bool, err error) {
	// One read for both keys: --get-regexp exits 1 when neither is set.
	out, err := localGit(ctx, gitDir, "config", "--local", "--get-regexp", `^remote\.origin\.(url|fetch)$`)
	if err != nil && !exitCode(err, 1) {
		return false, err
	}
	hasURL, hasFetch := false, false
	for _, line := range strings.Split(out, "\n") {
		key, _, _ := strings.Cut(line, " ")
		switch strings.ToLower(key) {
		case "remote.origin.url":
			hasURL = true
		case "remote.origin.fetch":
			hasFetch = true
		}
	}
	if !hasURL || hasFetch {
		return false, nil
	}
	if _, err = localGit(ctx, gitDir, "config", "--local", "--add", "remote.origin.fetch", OriginFetch); err != nil {
		return false, err
	}
	return true, nil
}

// MigrateOriginFetch gives every source copy this importer owns (its own
// directory under the root, with its ownership record) the origin refspec it
// lacks (EnsureOriginFetch). The Runner runs it on start; a repository the
// person registered from their own checkout is never touched. It returns how
// many it changed.
func (i *Importer) MigrateOriginFetch(ctx context.Context) (int, error) {
	changed := 0
	var errs []error
	for _, r := range i.library.Repositories() {
		if !i.owns(r) {
			continue
		}
		added, err := EnsureOriginFetch(ctx, r.Path)
		if err != nil {
			errs = append(errs, fmt.Errorf("%s: %w", r.ID, err))
		}
		if added {
			changed++
		}
	}
	return changed, errors.Join(errs...)
}

// owns reports a source copy this importer made: at root/<id>/source.git,
// with the ownership record it wrote there.
func (i *Importer) owns(r collections.Repository) bool {
	if r.ID == "" || !samePath(r.Path, filepath.Join(i.root, r.ID, "source.git")) {
		return false
	}
	info, err := os.Lstat(filepath.Join(r.Path, "lazybox-source.json"))
	if err != nil || !info.Mode().IsRegular() || info.Size() > 8192 {
		return false
	}
	data, err := os.ReadFile(filepath.Join(r.Path, "lazybox-source.json"))
	var saved collections.Repository
	return err == nil && json.Unmarshal(data, &saved) == nil && saved.ID == r.ID && samePath(saved.Path, r.Path)
}

// samePath compares two paths as the file system resolves them (the library
// keeps a repository's path resolved: /var is /private/var on macOS).
func samePath(a, b string) bool {
	resolve := func(p string) string {
		if resolved, err := filepath.EvalSymlinks(p); err == nil {
			return resolved
		}
		return filepath.Clean(p)
	}
	return resolve(a) == resolve(b)
}

// localGit runs git on gitDir with no ambient Git environment or config (a
// GIT_DIR or a global setting cannot redirect it), bounded in time.
func localGit(ctx context.Context, gitDir string, args ...string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, "git", append([]string{"--git-dir=" + gitDir}, args...)...)
	for _, entry := range os.Environ() {
		if key, _, _ := strings.Cut(entry, "="); !strings.HasPrefix(key, "GIT_") {
			cmd.Env = append(cmd.Env, entry)
		}
	}
	cmd.Env = append(cmd.Env, "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull, "GIT_TERMINAL_PROMPT=0")
	out, err := cmd.Output()
	return strings.TrimSpace(string(out)), err
}

func exitCode(err error, code int) bool {
	var exit *exec.ExitError
	return errors.As(err, &exit) && exit.ExitCode() == code
}
