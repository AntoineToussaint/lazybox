// Package repositories owns private source checkouts used to create worktrees.
package repositories

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
)

type Cloner interface {
	Clone(context.Context, string, string, string) error
}

// ProgressCloner optionally reports sanitized transfer stages without exposing Git stderr.
type ProgressCloner interface {
	CloneWithProgress(context.Context, string, string, string, func(string) error) error
}
type Git struct{}

// Clone places authentication only in this child's scoped HTTP configuration.
// No token appears in argv, the origin URL, a credential helper, or Git config.
func (g Git) Clone(ctx context.Context, source, destination, token string) error {
	return g.CloneWithProgress(ctx, source, destination, token, nil)
}
func (Git) CloneWithProgress(ctx context.Context, source, destination, token string, progress func(string) error) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	cmd := exec.CommandContext(ctx, "git", "-c", "credential.helper=", "-c", "http.followRedirects=false", "clone", "--bare", "--progress", "--", source, destination)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = time.Second
	for _, entry := range os.Environ() {
		key, _, _ := strings.Cut(entry, "=")
		if strings.HasPrefix(key, "GIT_") || strings.HasPrefix(key, "GH_") || strings.HasPrefix(key, "GITHUB_") {
			continue
		}
		cmd.Env = append(cmd.Env, entry)
	}
	cmd.Env = append(cmd.Env, "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull, "GIT_TERMINAL_PROMPT=0", "GIT_ASKPASS=", "GIT_CONFIG_COUNT=1", "GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader", "GIT_CONFIG_VALUE_0=Authorization: Basic "+base64.StdEncoding.EncodeToString([]byte("x-access-token:"+token)))
	if progress != nil {
		cmd.Stderr = &progressWriter{emit: progress, cancel: cancel}
	}
	if err := cmd.Run(); err != nil {
		return errors.New("GitHub clone failed; check repository access and network connectivity")
	}
	return nil
}

type Library interface {
	Repositories() []collections.Repository
	RegisterRepository(context.Context, collections.Repository) (collections.Repository, error)
	ReplaceRepository(context.Context, collections.Repository) (collections.Repository, []collections.Repository, error)
}
type Importer struct {
	mu      sync.Mutex
	root    string
	library Library
	git     Cloner
}

func New(root string, library Library, git Cloner) (*Importer, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("absolute repository root required")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	return &Importer{root: root, library: library, git: git}, nil
}

var namePattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`)

func Validate(repo github.Repository) error {
	parts := strings.Split(repo.FullName, "/")
	if repo.ID <= 0 || len(parts) != 2 || !namePattern.MatchString(parts[0]) || !namePattern.MatchString(parts[1]) || repo.Name != parts[1] || repo.Disabled {
		return errors.New("GitHub returned an unsupported repository identity")
	}
	if repo.CloneURL != "https://github.com/"+repo.FullName+".git" || repo.HTMLURL != "https://github.com/"+repo.FullName {
		return errors.New("GitHub repository URLs must match its canonical identity")
	}
	return nil
}
func (i *Importer) Import(ctx context.Context, repo github.Repository, token string) (collections.Repository, error) {
	return i.ImportWithProgress(ctx, repo, token, nil)
}
func (i *Importer) ImportWithProgress(ctx context.Context, repo github.Repository, token string, progress func(string) error) (collections.Repository, error) {
	i.mu.Lock()
	defer i.mu.Unlock()
	if err := Validate(repo); err != nil {
		return collections.Repository{}, err
	}
	id := "github-" + strconv.FormatInt(repo.ID, 10)
	for _, r := range i.library.Repositories() {
		if gitidentity.GitHubName(r.SourceURL) == strings.ToLower(repo.FullName) {
			return r, nil
		}
		if r.ID == id {
			if r.GitHubID != repo.ID {
				return collections.Repository{}, errors.New("repository identity collision")
			}
			return r, nil
		}
	}
	destination := filepath.Join(i.root, id)
	record := collections.Repository{ID: id, Name: repo.Name, Path: filepath.Join(destination, "source.git"), SourceURL: repo.HTMLURL, GitHubID: repo.ID}
	// A completed clone with its own marker can be enrolled after a crash between
	// rename and registration. Unknown paths are never adopted or overwritten.
	if info, err := os.Lstat(destination); err == nil {
		if !info.IsDir() || info.Mode()&os.ModeSymlink != 0 {
			return collections.Repository{}, errors.New("repository destination must be an owned directory")
		}
		markerPath := filepath.Join(record.Path, "lazybox-source.json")
		markerInfo, statErr := os.Lstat(markerPath)
		if statErr != nil || !markerInfo.Mode().IsRegular() || markerInfo.Size() > 8192 {
			return collections.Repository{}, errors.New("repository destination has no valid ownership record")
		}
		data, readErr := os.ReadFile(markerPath)
		var saved collections.Repository
		if readErr != nil || json.Unmarshal(data, &saved) != nil || saved != record {
			return collections.Repository{}, errors.New("repository destination exists without a matching Lazybox ownership record")
		}
		if _, err = EnsureOriginFetch(ctx, record.Path); err != nil {
			return collections.Repository{}, err
		}
		return i.library.RegisterRepository(ctx, record)
	} else if !errors.Is(err, os.ErrNotExist) {
		return collections.Repository{}, err
	}
	if err := i.clone(ctx, repo, token, progress, destination, record); err != nil {
		return collections.Repository{}, err
	}
	return i.library.RegisterRepository(ctx, record)
}

// Replace makes a fresh copy of repo, as GitHub has it now, and registers it
// in place of the copy the library has under its name (ReplaceRepository).
// The old copy stays on disk, hidden: workspaces made from it keep their
// worktrees, which Git registered in it. It is for a repository whose
// history changed on GitHub since Lazybox copied it: a force-push with a new
// root, or a new repository that took the name. It answers the new copy and
// the ones it replaced.
func (i *Importer) Replace(ctx context.Context, repo github.Repository, token string, progress func(string) error) (collections.Repository, []collections.Repository, error) {
	i.mu.Lock()
	defer i.mu.Unlock()
	if err := Validate(repo); err != nil {
		return collections.Repository{}, nil, err
	}
	taken := map[string]bool{}
	for _, r := range i.library.Repositories() {
		taken[r.ID] = true
	}
	// A new directory and registration of its own: the old ID, if it is this
	// repository's, stays the old copy's.
	base := "github-" + strconv.FormatInt(repo.ID, 10)
	id := base
	for n := 2; ; n++ {
		if _, err := os.Lstat(filepath.Join(i.root, id)); errors.Is(err, os.ErrNotExist) && !taken[id] && !i.registered(id) {
			break
		}
		if n > 100 {
			return collections.Repository{}, nil, errors.New("too many replaced copies of this repository")
		}
		id = base + "-" + strconv.Itoa(n)
	}
	destination := filepath.Join(i.root, id)
	record := collections.Repository{ID: id, Name: repo.Name, Path: filepath.Join(destination, "source.git"), SourceURL: repo.HTMLURL, GitHubID: repo.ID}
	if err := i.clone(ctx, repo, token, progress, destination, record); err != nil {
		return collections.Repository{}, nil, err
	}
	return i.library.ReplaceRepository(ctx, record)
}

// registered reports a registration with id, hidden ones included, where the
// library can say.
func (i *Importer) registered(id string) bool {
	if lookup, ok := i.library.(interface {
		Repository(string) (collections.Repository, bool)
	}); ok {
		_, found := lookup.Repository(id)
		return found
	}
	return false
}

// Owns reports a source copy this importer made (as opposed to a checkout the
// person registered, which Lazybox never replaces).
func (i *Importer) Owns(r collections.Repository) bool { return i.owns(r) }

// clone makes the bare copy of repo at destination/source.git with its
// ownership record, staged and renamed into place so a crash leaves either
// nothing or a complete copy.
func (i *Importer) clone(ctx context.Context, repo github.Repository, token string, progress func(string) error, destination string, record collections.Repository) error {
	staging, err := os.MkdirTemp(i.root, ".clone-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(staging)
	clone := filepath.Join(staging, "source.git")
	if reporting, ok := i.git.(ProgressCloner); ok && progress != nil {
		err = reporting.CloneWithProgress(ctx, repo.CloneURL, clone, token, progress)
	} else {
		err = i.git.Clone(ctx, repo.CloneURL, clone, token)
	}
	if err != nil {
		return err
	}
	// A pushed worktree branch gets its remote-tracking ref (fetchspec.go).
	if _, err = EnsureOriginFetch(ctx, clone); err != nil {
		return err
	}
	data, err := json.Marshal(record)
	if err != nil {
		return err
	}
	marker, err := os.OpenFile(filepath.Join(clone, "lazybox-source.json"), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	_, err = marker.Write(data)
	if err == nil {
		err = marker.Sync()
	}
	err = errors.Join(err, marker.Close())
	if err != nil {
		return err
	}
	// Reserve the parent exclusively. Rename then targets our own new directory,
	// so an external checkout appearing concurrently cannot be overwritten.
	if err = os.Mkdir(destination, 0700); err != nil {
		return errors.New("repository destination appeared during import; refusing to overwrite")
	}
	if err = os.Rename(clone, record.Path); err != nil {
		_ = os.Remove(destination)
		return err
	}
	parent, err := os.Open(destination)
	if err != nil {
		return err
	}
	err = parent.Sync()
	_ = parent.Close()
	return err
}
