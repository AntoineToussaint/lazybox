package collections

import (
	"bufio"

	"context"
	"errors"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode"
	"unicode/utf16"
	"unicode/utf8"
)

// The file index is what the finder and link resolution search: every file Git
// would show in each repository (tracked, plus untracked that .gitignore does
// not exclude), prefixed with the repository folder, plus regular files at the
// workspace root. It is bounded in size and time and cached briefly, so typing
// in the finder does not run Git per keystroke.
var (
	// Variables only so tests can exercise the bounds on small fixtures.
	indexLimit = 200_000
	indexTTL   = 10 * time.Second
)

const (
	indexTimeout      = 10 * time.Second
	searchLimit       = 200
	searchDefault     = 50
	resolveLimit      = 50
	queryLimit        = 256
	suffixComponents  = 32
	referenceMaxBytes = 4096
)

type FileMatch struct {
	Path string
	// Positions index the matched characters of Path in UTF-16 code units, the
	// unit browsers index strings by.
	Positions []uint32
}

type PathMatch struct {
	Path      string
	Directory bool
}

type fileIndex struct {
	built     time.Time
	paths     []string
	truncated bool
}

// indexCache holds each workspace's file index; the zero value is ready.
type indexCache struct {
	mu    sync.Mutex
	items map[string]*fileIndex
	// building serializes index builds per workspace, so concurrent keystrokes
	// share one Git run instead of starting several.
	building map[string]*sync.Mutex
}

// workspace returns a snapshot of a readable workspace whose root is still the
// folder this Manager created. Callers search the snapshot without holding m.mu.
func (m *Manager) workspace(id string) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok || m.closed || c.State == "deleted" || c.State == "deleting" {
		return Collection{}, errors.New("workspace files are unavailable")
	}
	if !validRoot(m.root, c) {
		return Collection{}, errors.New("workspace root is invalid")
	}
	resolved, err := filepath.EvalSymlinks(c.Root)
	if err != nil || resolved != c.Root {
		return Collection{}, errors.New("workspace root moved or was replaced")
	}
	c.Members = append([]Member(nil), c.Members...)
	return c, nil
}

func (m *Manager) index(ctx context.Context, c Collection) (*fileIndex, error) {
	cache := &m.files
	cache.mu.Lock()
	if cache.items == nil {
		cache.items, cache.building = map[string]*fileIndex{}, map[string]*sync.Mutex{}
	}
	build := cache.building[c.ID]
	if build == nil {
		build = &sync.Mutex{}
		cache.building[c.ID] = build
	}
	cache.mu.Unlock()
	build.Lock()
	defer build.Unlock()
	cache.mu.Lock()
	current := cache.items[c.ID]
	cache.mu.Unlock()
	if current != nil && time.Since(current.built) < indexTTL {
		return current, nil
	}
	ctx, cancel := context.WithTimeout(ctx, indexTimeout)
	defer cancel()
	stop := context.AfterFunc(m.ctx, cancel)
	defer stop()
	next := &fileIndex{built: time.Now()}
	add := func(p string) bool {
		if len(next.paths) >= indexLimit {
			next.truncated = true
			return false
		}
		next.paths = append(next.paths, p)
		return true
	}
	if entries, err := os.ReadDir(c.Root); err == nil {
		for _, entry := range entries {
			if entry.Type().IsRegular() && !strings.HasPrefix(entry.Name(), ".lazybox") {
				add(entry.Name())
			}
		}
	}
	members := append([]Member(nil), c.Members...)
	sort.Slice(members, func(i, j int) bool { return members[i].Name < members[j].Name })
	for _, member := range members {
		if !member.Ready || !component.MatchString(member.Name) || next.truncated {
			continue
		}
		folder := filepath.Join(c.Root, member.Name)
		if err := listRepository(ctx, folder, func(relative string) bool { return add(member.Name + "/" + relative) }); err != nil {
			if ctx.Err() != nil {
				return nil, ctx.Err()
			}
			// Not a Git checkout (or Git failed): fall back to a bounded walk
			// that skips the folders Git would.
			walkFolder(ctx, folder, func(relative string) bool { return add(member.Name + "/" + relative) })
		}
	}
	cache.mu.Lock()
	cache.items[c.ID] = next
	cache.mu.Unlock()
	return next, nil
}

// listRepository streams `git ls-files` so the index never buffers more than
// it keeps, and stops Git as soon as the bound is reached.
func listRepository(ctx context.Context, folder string, add func(string) bool) error {
	cmd := exec.CommandContext(ctx, "git", "-C", folder, "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--deduplicate")
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = time.Second
	cmd.Env = gitEnvironment()
	out, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	if err = cmd.Start(); err != nil {
		return err
	}
	reader := bufio.NewReaderSize(out, 64<<10)
	full := false
	for !full {
		entry, err := reader.ReadBytes(0)
		if len(entry) > 0 && entry[len(entry)-1] == 0 {
			entry = entry[:len(entry)-1]
		}
		if len(entry) > 0 && utf8.Valid(entry) {
			full = !add(string(entry))
		}
		if err != nil {
			break
		}
	}
	if full {
		_ = cmd.Cancel()
		_, _ = io.Copy(io.Discard, out)
		_ = cmd.Wait()
		return nil
	}
	return cmd.Wait()
}

func gitEnvironment() []string {
	env := []string{}
	for _, e := range os.Environ() {
		key, _, _ := strings.Cut(e, "=")
		if !strings.HasPrefix(key, "GIT_") {
			env = append(env, e)
		}
	}
	return append(env, "GIT_TERMINAL_PROMPT=0")
}

var skippedFolders = map[string]bool{".git": true, "node_modules": true, ".cache": true, "dist": true, "target": true, "vendor": true, ".venv": true, "__pycache__": true}

func walkFolder(ctx context.Context, folder string, add func(string) bool) {
	_ = filepath.WalkDir(folder, func(p string, entry fs.DirEntry, err error) error {
		if err != nil || ctx.Err() != nil {
			return filepath.SkipDir
		}
		if entry.IsDir() {
			if p != folder && skippedFolders[entry.Name()] {
				return filepath.SkipDir
			}
			return nil
		}
		if !entry.Type().IsRegular() {
			return nil
		}
		relative, err := filepath.Rel(folder, p)
		if err != nil || !add(filepath.ToSlash(relative)) {
			return filepath.SkipAll
		}
		return nil
	})
}

// SearchFiles ranks the workspace's files against a fuzzy query, the way an
// editor's "go to file" does: characters in order, preferring the file name,
// word starts and runs of adjacent characters.
func (m *Manager) SearchFiles(ctx context.Context, id, query string, limit int) ([]FileMatch, bool, int, error) {
	c, err := m.workspace(id)
	if err != nil {
		return nil, false, 0, err
	}
	query = strings.Join(strings.Fields(query), "")
	if len(query) > queryLimit {
		return nil, false, 0, errors.New("search query is too long")
	}
	if limit <= 0 {
		limit = searchDefault
	}
	limit = min(limit, searchLimit)
	index, err := m.index(ctx, c)
	if err != nil {
		return nil, false, 0, err
	}
	if query == "" {
		return nil, index.truncated, len(index.paths), nil
	}
	needle := []rune(strings.ToLower(query))
	type scored struct {
		path      string
		score     int
		positions []int
	}
	var results []scored
	for i, candidate := range index.paths {
		if i%4096 == 0 && ctx.Err() != nil {
			return nil, false, 0, ctx.Err()
		}
		if score, positions, ok := fuzzyScore(candidate, needle); ok {
			results = append(results, scored{candidate, score, positions})
		}
	}
	sort.Slice(results, func(i, j int) bool {
		a, b := results[i], results[j]
		if a.score != b.score {
			return a.score > b.score
		}
		if len(a.path) != len(b.path) {
			return len(a.path) < len(b.path)
		}
		return a.path < b.path
	})
	root, err := os.OpenRoot(c.Root)
	if err != nil {
		return nil, false, 0, err
	}
	defer root.Close()
	matches := []FileMatch{}
	for _, result := range results {
		if len(matches) >= limit {
			break
		}
		// Only what opens inside the workspace is offered: a tracked symlink to a
		// file elsewhere, or a file deleted since indexing, is left out.
		if info, err := root.Stat(filepath.FromSlash(result.path)); err != nil || !info.Mode().IsRegular() {
			continue
		}
		matches = append(matches, FileMatch{Path: result.path, Positions: utf16Positions(result.path, result.positions)})
	}
	return matches, index.truncated, len(index.paths), nil
}

func utf16Positions(text string, runes []int) []uint32 {
	out := make([]uint32, 0, len(runes))
	offsets := make([]uint32, 0, len(text))
	var unit uint32
	for _, r := range text {
		offsets = append(offsets, unit)
		unit += uint32(len(utf16.Encode([]rune{r})))
	}
	for _, i := range runes {
		if i < len(offsets) {
			out = append(out, offsets[i])
		}
	}
	return out
}

func boundary(previous, current rune) bool {
	return previous == '/' || previous == '_' || previous == '-' || previous == '.' || previous == ' ' || (unicode.IsLower(previous) && unicode.IsUpper(current))
}

// fuzzyScore matches needle (lower case) as a subsequence of candidate. When
// the file name alone contains the needle, the match is confined to it.
func fuzzyScore(candidate string, needle []rune) (int, []int, bool) {
	text := []rune(candidate)
	lower := make([]rune, len(text))
	for i, r := range text {
		lower[i] = unicode.ToLower(r)
	}
	base := 0
	if slash := strings.LastIndexByte(candidate, '/'); slash >= 0 {
		base = utf8.RuneCountInString(candidate[:slash+1])
	}
	hasSlash := false
	for _, r := range needle {
		if r == '/' {
			hasSlash = true
		}
	}
	if !hasSlash {
		if positions, ok := window(lower, needle, base); ok {
			score := score(text, positions, base) + 40
			name := string(lower[base:])
			if name == string(needle) {
				score += 100
			} else if strings.HasPrefix(name, string(needle)) {
				score += 30
			} else if stem, _, found := strings.Cut(name, "."); found && stem == string(needle) {
				score += 80
			}
			return score, positions, true
		}
	}
	positions, ok := window(lower, needle, 0)
	if !ok {
		return 0, nil, false
	}
	return score(text, positions, base), positions, true
}

// window finds the shortest-ending match: a forward pass finds where the
// needle can first end, a backward pass from there pulls the start as late as
// possible, so "fbar" in "foo/bar" picks the f of foo and the bar of bar.
func window(text, needle []rune, from int) ([]int, bool) {
	if len(needle) == 0 {
		return nil, false
	}
	j, end := 0, -1
	for i := from; i < len(text); i++ {
		if text[i] == needle[j] {
			j++
			if j == len(needle) {
				end = i
				break
			}
		}
	}
	if end < 0 {
		return nil, false
	}
	positions := make([]int, len(needle))
	j = len(needle) - 1
	for i := end; i >= from && j >= 0; i-- {
		if text[i] == needle[j] {
			positions[j] = i
			j--
		}
	}
	// Prefer word starts: move each matched character to an equal character at
	// a boundary later in its slot when one exists.
	for k := range positions {
		limit := len(text) - 1
		if k+1 < len(positions) {
			limit = positions[k+1] - 1
		}
		if positions[k] > from && boundary(text[positions[k]-1], text[positions[k]]) {
			continue
		}
		for i := positions[k] + 1; i <= limit; i++ {
			if text[i] == text[positions[k]] && i > 0 && boundary(text[i-1], text[i]) {
				positions[k] = i
				break
			}
		}
	}
	return positions, true
}

func score(text []rune, positions []int, base int) int {
	total := 0
	for k, i := range positions {
		total += 16
		if i == 0 || boundary(text[i-1], text[i]) {
			total += 10
			if i > 0 && text[i-1] == '/' {
				total += 2
			}
		}
		if i == base && k == 0 {
			total += 15
		}
		if k > 0 {
			if positions[k-1] == i-1 {
				total += 8
			} else {
				total -= min(i-positions[k-1]-1, 12)
			}
		}
	}
	return total - len(text)/8
}

// ResolvePath finds what a file reference names in this workspace. A reference
// is what an agent wrote: a path relative to one repository, to the workspace,
// to some other checkout, or a bare file name. It never reads outside the
// workspace: every candidate is opened through os.Root.
func (m *Manager) ResolvePath(ctx context.Context, id, reference string) ([]PathMatch, bool, bool, error) {
	c, err := m.workspace(id)
	if err != nil {
		return nil, false, false, err
	}
	reference = strings.TrimSpace(reference)
	if reference == "" || len(reference) > referenceMaxBytes || strings.ContainsRune(reference, 0) {
		return nil, false, false, errors.New("valid file reference required")
	}
	reference = filepath.ToSlash(reference)
	if filepath.IsAbs(reference) {
		if relative, err := filepath.Rel(c.Root, reference); err == nil && filepath.IsLocal(relative) {
			reference = filepath.ToSlash(relative)
		}
	}
	// What is left after cleaning, with any leading / or ../ dropped: an
	// absolute path from another machine or a path above the agent's folder is
	// looked up by its tail inside this workspace, never followed outside it.
	parts := []string{}
	for _, part := range strings.Split(path.Clean("/"+reference), "/") {
		if part != "" && part != "." && part != ".." {
			parts = append(parts, part)
		}
	}
	root, err := os.OpenRoot(c.Root)
	if err != nil {
		return nil, false, false, err
	}
	defer root.Close()
	if len(parts) == 0 {
		return []PathMatch{{Path: ".", Directory: true}}, true, false, nil
	}
	stat := func(p string) (PathMatch, bool) {
		info, err := root.Stat(filepath.FromSlash(p))
		if err != nil || (!info.IsDir() && !info.Mode().IsRegular()) {
			return PathMatch{}, false
		}
		return PathMatch{Path: p, Directory: info.IsDir()}, true
	}
	members := []string{}
	for _, member := range c.Members {
		if component.MatchString(member.Name) {
			members = append(members, member.Name)
		}
	}
	sort.Strings(members)
	// Exact: the reference, then each shorter tail of it, at the workspace root
	// and under each repository. The longest tail that names something wins. A
	// bare name counts as exact only when the reference was that bare name;
	// otherwise the name search below sees every file of that name.
	for start := 0; start < len(parts) && start < suffixComponents && (start == 0 || len(parts)-start > 1); start++ {
		tail := strings.Join(parts[start:], "/")
		found := []PathMatch{}
		if match, ok := stat(tail); ok {
			found = append(found, match)
		}
		for _, member := range members {
			// api/x under member api is the reference itself, not api/api/x.
			if tail == member || strings.HasPrefix(tail, member+"/") {
				continue
			}
			if match, ok := stat(member + "/" + tail); ok {
				found = append(found, match)
			}
		}
		if len(found) > 0 {
			return found, true, false, nil
		}
		if err := ctx.Err(); err != nil {
			return nil, false, false, err
		}
	}
	index, err := m.index(ctx, c)
	if err != nil {
		return nil, false, false, err
	}
	collect := func(match func(string) (string, bool)) ([]PathMatch, bool) {
		seen := map[string]bool{}
		found := []PathMatch{}
		for _, candidate := range index.paths {
			p, ok := match(candidate)
			if !ok || seen[p] {
				continue
			}
			seen[p] = true
			if result, ok := stat(p); ok {
				if len(found) == resolveLimit {
					return found, true
				}
				found = append(found, result)
			}
		}
		sort.Slice(found, func(i, j int) bool {
			if len(found[i].Path) != len(found[j].Path) {
				return len(found[i].Path) < len(found[j].Path)
			}
			return found[i].Path < found[j].Path
		})
		return found, false
	}
	// By path suffix, then by name: files first, then folders of that name.
	for start := 0; start < len(parts) && start < suffixComponents; start++ {
		if len(parts)-start < 2 {
			break
		}
		tail := strings.Join(parts[start:], "/")
		needle := "/" + tail
		found, truncated := collect(func(candidate string) (string, bool) {
			if strings.HasSuffix("/"+candidate, needle) {
				return candidate, true
			}
			if i := strings.Index("/"+candidate+"/", needle+"/"); i >= 0 {
				return candidate[:i+len(tail)], true
			}
			return "", false
		})
		if len(found) > 0 {
			return found, false, truncated, nil
		}
	}
	name := parts[len(parts)-1]
	found, truncated := collect(func(candidate string) (string, bool) {
		return candidate, path.Base(candidate) == name
	})
	if len(found) == 0 {
		found, truncated = collect(func(candidate string) (string, bool) {
			if i := strings.Index("/"+candidate, "/"+name+"/"); i >= 0 {
				return candidate[:i+len(name)], true
			}
			return "", false
		})
	}
	return found, false, truncated, nil
}
