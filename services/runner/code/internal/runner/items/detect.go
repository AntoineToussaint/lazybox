package items

import (
	"encoding/json"
	"regexp"
	"strconv"
	"strings"
)

// Agent output names pull requests in two ways. A command that creates one
// (`gh pr create`, `hub pull-request`, `glab mr create`, a GitHub tool's
// create_pull_request) prints the new pull request's URL: that URL is a
// pull request the workspace delivers. Any other recognized URL is only a
// mention. `git push` prints a "Create a pull request" hint whose URL
// (/pull/new/<branch>) is not a pull request, so it never matches.

// Resolved is a URL a provider recognized.
type Resolved struct {
	Ref  Ref
	Kind string
	// Repository is owner/name; Number the pull request or issue number.
	Repository string
	Number     int64
	// URL is the item's canonical URL (no fragment, no sub-page).
	URL string
}

var githubItemURL = regexp.MustCompile(`https://github\.com/([A-Za-z0-9](?:[A-Za-z0-9-]{0,38}))/([A-Za-z0-9._-]{1,100})/(pull|issues)/([1-9][0-9]{0,9})\b`)

// ResolveGitHub maps a GitHub pull request or issue URL to its item, without
// the network. A pull request's sub-pages (/files, #discussion) resolve to it.
func ResolveGitHub(url string) (Resolved, bool) {
	m := githubItemURL.FindStringSubmatch(strings.TrimSpace(url))
	if m == nil || !strings.HasPrefix(strings.TrimSpace(url), m[0]) {
		return Resolved{}, false
	}
	return githubMatch(m)
}

// githubMatch is the item a URL match names; false for a repository name
// GitHub never has ("", ".", ".."), which would name another API path.
func githubMatch(m []string) (Resolved, bool) {
	owner, name, kind := m[1], strings.TrimSuffix(m[2], ".git"), KindPullRequest
	if !RepositoryName(name) {
		return Resolved{}, false
	}
	if m[3] == "issues" {
		kind = KindIssue
	}
	number, _ := strconv.ParseInt(m[4], 10, 64)
	repository := owner + "/" + name
	return Resolved{Ref: GitHubRef(repository, number), Kind: kind, Repository: repository, Number: number, URL: "https://github.com/" + repository + "/" + m[3] + "/" + m[4]}, true
}

// RepositoryName reports a name GitHub may give a repository: not empty,
// "." or "..", which in an API path would name another endpoint.
func RepositoryName(name string) bool {
	return name != "" && name != "." && name != ".."
}

// GitHubRef is the ref of a GitHub pull request or issue. GitHub numbers
// both in one sequence per repository, so the key needs no kind.
func GitHubRef(repository string, number int64) Ref {
	return Ref{Provider: "github", Key: strings.ToLower(repository) + "#" + strconv.FormatInt(number, 10)}
}

// ParseGitHubKey splits owner/name#number.
func ParseGitHubKey(key string) (string, int64, bool) {
	repository, number, ok := strings.Cut(key, "#")
	if !ok || strings.Count(repository, "/") != 1 {
		return "", 0, false
	}
	if owner, name, _ := strings.Cut(repository, "/"); !RepositoryName(owner) || !RepositoryName(name) {
		return "", 0, false
	}
	n, err := strconv.ParseInt(number, 10, 64)
	return repository, n, err == nil && n > 0
}

var creates = regexp.MustCompile(`(?:^|[^A-Za-z0-9_-])(?:gh\s+pr\s+create|hub\s+pull-request|glab\s+mr\s+create)\b|create_pull_request`)

// CreatesPullRequest reports a command line (or a tool's name and input)
// that opens a pull request.
func CreatesPullRequest(command string) bool {
	return creates.MatchString(command)
}

// ToolCommand is what a tool call runs, for CreatesPullRequest: its name and
// the strings of its input, decoded. Matched as raw JSON, a command on its
// own line after a heredoc follows the escape `\n` and goes unseen.
func ToolCommand(name string, input []byte) string {
	var value any
	if json.Unmarshal(input, &value) != nil {
		return name + " " + string(input)
	}
	parts := []string{name}
	var walk func(any)
	walk = func(v any) {
		switch v := v.(type) {
		case string:
			parts = append(parts, v)
		case []any:
			for _, item := range v {
				walk(item)
			}
		case map[string]any:
			for _, item := range v {
				walk(item)
			}
		}
	}
	walk(value)
	return strings.Join(parts, "\n")
}

// Found is a URL in output, and whether the output's command created it.
type Found struct {
	Resolved
	Role string
}

// maxFound bounds what one output contributes: a listing of many pull
// requests is not a list of things the workspace did.
const maxFound = 5

// Scan finds the items an output names. created says the output is from a
// command that opens a pull request: the pull request URL it printed last is
// delivered (gh prints its progress first and the new URL last); every other
// URL is mentioned.
func Scan(output string, created bool) []Found {
	output = strings.ReplaceAll(output, `\/`, `/`)
	var all []Found
	seen := map[string]bool{}
	for _, m := range githubItemURL.FindAllStringSubmatch(output, -1) {
		r, ok := githubMatch(m)
		if ok && !seen[r.Ref.ID()] {
			seen[r.Ref.ID()] = true
			all = append(all, Found{Resolved: r, Role: RoleMentions})
		}
	}
	delivered := -1
	if created {
		for i, f := range all {
			if f.Kind == KindPullRequest {
				delivered = i
			}
		}
	}
	var out []Found
	if delivered >= 0 {
		all[delivered].Role = RoleDelivers
		out = append(out, all[delivered])
	}
	for i, f := range all {
		if i != delivered && len(out) < maxFound {
			out = append(out, f)
		}
	}
	return out
}
