package repositorycatalog

import (
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"unicode"
)

// Candidate is a catalog entry that matches what an agent asked for.
type Candidate struct {
	Entry
	// Score is 1–100: 100 is the exact repository, 40 a loose match.
	Score int
	// Source is where the entry came from: registered (a repository Lazybox
	// already prepared), local (a discovered folder) or github.
	Source string
}

// MinScore is the weakest match worth offering.
const MinScore = 40

// SourceOf names where an entry came from.
func SourceOf(e Entry) string {
	switch {
	case e.RepositoryID != "":
		return "registered"
	case e.LocalPath != "":
		return "local"
	case e.GitHubID != 0:
		return "github"
	}
	return "catalog"
}

var sourceRank = map[string]int{"registered": 0, "local": 1, "github": 2, "catalog": 3}

// Match ranks entries against a query, best first, at most limit: a
// repository name, owner/repo, github:owner/repo, a GitHub URL (https or ssh)
// or an absolute local path. Matching is case-insensitive and forgiving about
// separators (module_directory finds module-directory) and small typos.
func Match(entries []Entry, query string, limit int) []Candidate {
	query = strings.TrimSpace(query)
	if query == "" || limit <= 0 {
		return nil
	}
	var out []Candidate
	if filepath.IsAbs(query) {
		path := filepath.Clean(query)
		for _, e := range entries {
			if e.LocalPath == "" {
				continue
			}
			local := filepath.Clean(e.LocalPath)
			switch {
			case path == local:
				out = append(out, Candidate{Entry: e, Score: 100})
			case strings.HasPrefix(path, local+string(filepath.Separator)):
				out = append(out, Candidate{Entry: e, Score: 95})
			}
		}
		// A path the catalog does not know still names a repository: match
		// its folder name, below any entry for the path itself.
		if len(out) == 0 {
			for _, c := range matchName(entries, filepath.Base(path)) {
				c.Score = min(c.Score, 80)
				out = append(out, c)
			}
		}
	} else {
		out = matchName(entries, query)
	}
	for i := range out {
		out[i].Source = SourceOf(out[i].Entry)
	}
	sort.SliceStable(out, func(i, j int) bool {
		if out[i].Score != out[j].Score {
			return out[i].Score > out[j].Score
		}
		if a, b := sourceRank[out[i].Source], sourceRank[out[j].Source]; a != b {
			return a < b
		}
		return out[i].Name < out[j].Name
	})
	if len(out) > limit {
		out = out[:limit]
	}
	return out
}

var (
	repositoryIDQuery = regexp.MustCompile(`^(?:(?:github[-:])?([0-9]+)|(?:local|github)-[0-9a-f]{64})$`)
	bareDigits        = regexp.MustCompile(`^[0-9]+$`)
)

func matchName(entries []Entry, query string) []Candidate {
	if id := repositoryIDQuery.FindStringSubmatch(strings.ToLower(query)); id != nil {
		var matches []Candidate
		for _, e := range entries {
			if strings.EqualFold(query, e.RepositoryID) || (e.GitHubID > 0 && strconv.FormatInt(e.GitHubID, 10) == id[1]) {
				matches = append(matches, Candidate{Entry: e, Score: 100})
			}
		}
		// Bare digits may also be a repository's name (2048): only an ID-shaped query that is no name stops here.
		if len(matches) > 0 || !bareDigits.MatchString(query) {
			return matches
		}
	}
	owner, repo := splitName(NormalizeName(query))
	if repo == "" {
		return nil
	}
	var out []Candidate
	for _, e := range entries {
		full := strings.ToLower(strings.TrimPrefix(e.Identity, "github:"))
		if !strings.HasPrefix(e.Identity, "github:") {
			full = strings.ToLower(e.Name)
		}
		entryOwner, entryRepo := splitName(full)
		if owner != "" {
			if owner == entryOwner && repo == entryRepo {
				out = append(out, Candidate{Entry: e, Score: 100})
			}
			continue
		}
		score := nameScore(repo, entryRepo)
		if e.LocalPath != "" {
			score = max(score, nameScore(repo, strings.ToLower(filepath.Base(e.LocalPath)))-5)
		}
		if score >= MinScore {
			out = append(out, Candidate{Entry: e, Score: score})
		}
	}
	return out
}

// NormalizeName reduces a repository reference to lower-case owner/repo (or
// just repo): it drops github:, URL schemes, the github.com host, an ssh
// user, a .git suffix and anything after owner/repo in a URL.
func NormalizeName(query string) string {
	q := strings.ToLower(strings.TrimSpace(query))
	q = strings.TrimPrefix(q, "github:")
	for _, prefix := range []string{"https://", "http://", "ssh://", "git://"} {
		q = strings.TrimPrefix(q, prefix)
	}
	q = strings.TrimPrefix(q, "git@")
	q = strings.TrimPrefix(q, "www.")
	if rest, ok := strings.CutPrefix(q, "github.com"); ok {
		q = strings.TrimLeft(rest, ":/")
		if parts := strings.Split(q, "/"); len(parts) > 2 {
			q = parts[0] + "/" + parts[1]
		}
	}
	q = strings.Trim(q, "/")
	return strings.TrimSuffix(q, ".git")
}

func splitName(name string) (owner, repo string) {
	if i := strings.LastIndexByte(name, '/'); i >= 0 {
		return name[:i], name[i+1:]
	}
	return "", name
}

// nameScore compares two repository names (without owner), 0 when unrelated.
func nameScore(query, name string) int {
	if query == "" || name == "" {
		return 0
	}
	if query == name {
		return 90
	}
	q, n := squash(query), squash(name)
	if q == "" || n == "" {
		return 0
	}
	if q == n {
		return 85
	}
	if distance := levenshtein(q, n); distance <= max(1, len(q)/6) {
		return 75 - 5*distance
	}
	if len(q) >= 3 && (strings.Contains(n, q) || strings.Contains(q, n) && len(n) >= 3) {
		// The closer the lengths, the better: "module" in "module-directory".
		shorter, longer := min(len(q), len(n)), max(len(q), len(n))
		return 50 + 20*shorter/longer
	}
	if len(q) >= 3 && subsequence(q, n) {
		return MinScore
	}
	return 0
}

// squash keeps letters and digits: separators and case do not distinguish names.
func squash(s string) string {
	return strings.Map(func(r rune) rune {
		if unicode.IsLetter(r) || unicode.IsDigit(r) {
			return unicode.ToLower(r)
		}
		return -1
	}, s)
}

func subsequence(needle, haystack string) bool {
	want := []rune(needle)
	i := 0
	for _, r := range haystack {
		if i < len(want) && want[i] == r {
			i++
		}
	}
	return i == len(want)
}

func levenshtein(a, b string) int {
	ra, rb := []rune(a), []rune(b)
	previous := make([]int, len(rb)+1)
	for j := range previous {
		previous[j] = j
	}
	for i := 1; i <= len(ra); i++ {
		current := make([]int, len(rb)+1)
		current[0] = i
		for j := 1; j <= len(rb); j++ {
			cost := 1
			if ra[i-1] == rb[j-1] {
				cost = 0
			}
			current[j] = min(previous[j]+1, current[j-1]+1, previous[j-1]+cost)
		}
		previous = current
	}
	return previous[len(rb)]
}
