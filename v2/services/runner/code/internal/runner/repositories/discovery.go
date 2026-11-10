package repositories

import (
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/gitidentity"
)

type Candidate struct{ Name, Path, GitIdentity, DiscoveredAt string }
type ScanResult struct {
	Candidates []Candidate
	Truncated  bool
	Skipped    int
}
type DiscoveryRoot struct {
	Path      string
	ScannedAt int64
	ScanResult
}
type Catalog struct {
	Version int
	Roots   []DiscoveryRoot
}

// Scanner performs discovery only. Registering or modifying a repository is a
// separate, explicit operation through the worktree library.
type Scanner interface {
	Scan(context.Context, string) (ScanResult, error)
}
type CatalogStore interface {
	Load() (Catalog, error)
	Save(Catalog) error
}
type Discovery struct {
	scan    sync.Mutex
	mu      sync.Mutex
	store   CatalogStore
	scanner Scanner
	catalog Catalog
}

func NewDiscovery(store CatalogStore, scanner Scanner) (*Discovery, error) {
	catalog, err := store.Load()
	if errors.Is(err, os.ErrNotExist) {
		catalog = Catalog{Version: 3}
	} else if err != nil {
		return nil, err
	}
	// Older catalogs identified checkouts or local Git stores. Preserve chosen
	// roots, but require refresh to resolve provider repository identities.
	if catalog.Version == 1 || catalog.Version == 2 {
		catalog.Version = 3
		for i := range catalog.Roots {
			catalog.Roots[i].Candidates = nil
			catalog.Roots[i].ScannedAt = 0
		}
		if err := store.Save(catalog); err != nil {
			return nil, err
		}
	}
	if catalog.Version != 3 || len(catalog.Roots) > 16 {
		return nil, errors.New("invalid repository discovery catalog")
	}
	for _, root := range catalog.Roots {
		if !filepath.IsAbs(root.Path) || len(root.Candidates) > 512 {
			return nil, errors.New("invalid repository discovery root")
		}
		for _, c := range root.Candidates {
			rel, err := filepath.Rel(root.Path, c.DiscoveredAt)
			if err != nil || !filepath.IsAbs(c.Path) || !filepath.IsAbs(c.DiscoveredAt) || c.GitIdentity == "" || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
				return nil, errors.New("invalid discovered repository path")
			}
		}
	}
	return &Discovery{store: store, scanner: scanner, catalog: catalog}, nil
}
func cloneCatalog(c Catalog) Catalog {
	out := c
	out.Roots = append([]DiscoveryRoot(nil), c.Roots...)
	for i := range out.Roots {
		out.Roots[i].Candidates = append([]Candidate(nil), c.Roots[i].Candidates...)
	}
	return out
}
func (d *Discovery) List() Catalog { d.mu.Lock(); defer d.mu.Unlock(); return cloneCatalog(d.catalog) }
func (d *Discovery) Scan(ctx context.Context, root string) (Catalog, error) {
	d.scan.Lock()
	defer d.scan.Unlock()
	if !filepath.IsAbs(root) {
		return Catalog{}, errors.New("choose an absolute repository folder on the Runner")
	}
	root, err := filepath.EvalSymlinks(root)
	if err != nil {
		return Catalog{}, errors.New("repository search folder is unavailable")
	}
	info, err := os.Stat(root)
	if err != nil || !info.IsDir() {
		return Catalog{}, errors.New("repository search path must be a folder")
	}
	next := d.List()
	index := -1
	for i, saved := range next.Roots {
		if saved.Path == root {
			index = i
		}
	}
	if index < 0 && len(next.Roots) >= 16 {
		return Catalog{}, errors.New("at most 16 repository search folders are supported")
	}
	ctx, cancel := context.WithTimeout(ctx, time.Minute)
	defer cancel()
	result, err := d.scanner.Scan(ctx, root)
	if err != nil {
		return Catalog{}, err
	}
	if err := ctx.Err(); err != nil {
		return Catalog{}, err
	}
	saved := DiscoveryRoot{Path: root, ScannedAt: time.Now().Unix(), ScanResult: result}
	if index < 0 {
		next.Roots = append(next.Roots, saved)
	} else {
		next.Roots[index] = saved
	}
	sort.Slice(next.Roots, func(i, j int) bool { return next.Roots[i].Path < next.Roots[j].Path })
	if err := d.store.Save(next); err != nil {
		return Catalog{}, errors.New("could not save repository discovery results")
	}
	d.mu.Lock()
	d.catalog = next
	d.mu.Unlock()
	return cloneCatalog(next), nil
}

type FileCatalogStore struct{ path string }

func NewCatalogStore(root string) (*FileCatalogStore, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("absolute Runner discovery state required")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	return &FileCatalogStore{path: filepath.Join(root, "catalog.json")}, nil
}
func (s *FileCatalogStore) Load() (Catalog, error) {
	var out Catalog
	info, err := os.Lstat(s.path)
	if err != nil {
		return out, err
	}
	if !info.Mode().IsRegular() || info.Size() > 4<<20 {
		return out, errors.New("invalid repository discovery file")
	}
	data, err := os.ReadFile(s.path)
	if err == nil {
		err = json.Unmarshal(data, &out)
	}
	return out, err
}
func (s *FileCatalogStore) Save(c Catalog) error {
	data, err := json.Marshal(c)
	if err != nil {
		return err
	}
	if len(data) > 4<<20 {
		return errors.New("repository discovery catalog is too large")
	}
	f, err := os.CreateTemp(filepath.Dir(s.path), ".catalog-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(data)
	if err == nil {
		err = f.Sync()
	}
	err = errors.Join(err, f.Close())
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), s.path)
}

// DirectoryScanner never follows descendant symlinks or enters repositories,
// hidden folders, dependency trees or build output. Limits bound explicit scans.
type DirectoryScanner struct{}

func (DirectoryScanner) Scan(ctx context.Context, root string) (ScanResult, error) {
	result := ScanResult{}
	directories := 0
	seen := map[string]bool{}
	err := filepath.WalkDir(root, func(path string, entry fs.DirEntry, walkErr error) error {
		if err := ctx.Err(); err != nil {
			return err
		}
		if walkErr != nil {
			if path == root {
				return walkErr
			}
			result.Skipped++
			return filepath.SkipDir
		}
		if !entry.IsDir() {
			return nil
		}
		if path != root {
			name := entry.Name()
			if strings.HasPrefix(name, ".") || name == "node_modules" || name == "target" || name == "vendor" || name == "vendors" || name == "dist" || name == "build" {
				return filepath.SkipDir
			}
		}
		directories++
		if directories > 10000 || len(result.Candidates) >= 512 {
			result.Truncated = true
			return filepath.SkipAll
		}
		rel, _ := filepath.Rel(root, path)
		if strings.Count(rel, string(filepath.Separator)) >= 6 {
			result.Truncated = true
			return filepath.SkipDir
		}
		_, marker := os.Lstat(filepath.Join(path, ".git"))
		_, head := os.Lstat(filepath.Join(path, "HEAD"))
		objects, obj := os.Stat(filepath.Join(path, "objects"))
		if marker != nil && (head != nil || obj != nil || !objects.IsDir()) {
			return nil
		}
		// Validate marker candidates with read-only Git inspection. Clear ambient
		// GIT_* overrides so another agent's shell cannot redirect this operation.
		bare, err := inspectGit(ctx, path, "rev-parse", "--is-bare-repository")
		if err != nil {
			result.Skipped++
			return filepath.SkipDir
		}
		locationFlag := "--show-toplevel"
		if bare == "true" {
			locationFlag = "--absolute-git-dir"
		}
		location, err := inspectGit(ctx, path, "rev-parse", locationFlag)
		if err != nil || filepath.Clean(location) != path {
			result.Skipped++
			return filepath.SkipDir
		}

		common, err := inspectGit(ctx, path, "rev-parse", "--path-format=absolute", "--git-common-dir")
		if err == nil {
			common, err = filepath.EvalSymlinks(common)
		}
		if err != nil {
			result.Skipped++
			return filepath.SkipDir
		}
		remote, _ := inspectGit(ctx, path, "config", "--get", "remote.origin.url")
		identity := gitidentity.Repository(common, remote)
		if seen[identity] {
			return filepath.SkipDir
		}
		// Git lists the main checkout (or bare source) first. Resolve its path
		// through Git instead of naming a repository after a linked worktree.
		worktrees, err := inspectGit(ctx, path, "worktree", "list", "--porcelain", "-z")
		if err != nil {
			result.Skipped++
			return filepath.SkipDir
		}
		first, _, _ := strings.Cut(worktrees, "\x00")
		primary, ok := strings.CutPrefix(first, "worktree ")
		if !ok || !filepath.IsAbs(primary) {
			result.Skipped++
			return filepath.SkipDir
		}
		name := strings.TrimSuffix(filepath.Base(primary), ".git")
		if fullName := gitidentity.GitHubName(remote); fullName != "" {
			name = fullName
		}
		source := path
		if canonical, resolveErr := filepath.EvalSymlinks(primary); resolveErr == nil {
			primaryCommon, inspectErr := inspectGit(ctx, canonical, "rev-parse", "--path-format=absolute", "--git-common-dir")
			if inspectErr == nil {
				primaryCommon, inspectErr = filepath.EvalSymlinks(primaryCommon)
				if inspectErr == nil && primaryCommon == common {
					source = canonical
				}
			}
		}
		seen[identity] = true
		result.Candidates = append(result.Candidates, Candidate{Name: name, Path: source, GitIdentity: identity, DiscoveredAt: path})
		return filepath.SkipDir
	})
	if err != nil {
		return ScanResult{}, err
	}
	sort.Slice(result.Candidates, func(i, j int) bool { return result.Candidates[i].Path < result.Candidates[j].Path })
	return result, nil
}

func inspectGit(ctx context.Context, path string, args ...string) (string, error) {
	probe, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	cmd := exec.CommandContext(probe, "git", append([]string{"-C", path}, args...)...)
	cmd.WaitDelay = time.Second
	for _, value := range os.Environ() {
		key, _, _ := strings.Cut(value, "=")
		if !strings.HasPrefix(key, "GIT_") {
			cmd.Env = append(cmd.Env, value)
		}
	}
	cmd.Env = append(cmd.Env, "GIT_TERMINAL_PROMPT=0", "GIT_OPTIONAL_LOCKS=0")
	output, err := cmd.Output()
	return strings.TrimSpace(string(output)), err
}

// Remove drops a repository from saved discovery results only. An explicit scan
// may discover it again; no checkout or remote repository is touched.
func (d *Discovery) Remove(identity string) (Catalog, error) {
	if identity == "" {
		return Catalog{}, errors.New("repository identity required")
	}
	d.scan.Lock()
	defer d.scan.Unlock()
	next := d.List()
	for i := range next.Roots {
		rows := []Candidate{}
		for _, candidate := range next.Roots[i].Candidates {
			if candidate.GitIdentity != identity {
				rows = append(rows, candidate)
			}
		}
		next.Roots[i].Candidates = rows
	}
	if err := d.store.Save(next); err != nil {
		return Catalog{}, err
	}
	d.mu.Lock()
	d.catalog = next
	d.mu.Unlock()
	return cloneCatalog(next), nil
}
