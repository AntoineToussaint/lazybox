package items

import (
	"encoding/json"
	"path/filepath"
	"regexp"
	"slices"

	"mvdan.cc/sh/v3/syntax"
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

// CreatesPullRequestArgv identifies a supported create invocation from the
// actual arguments. Help, preview and browser modes do not deliver a PR.
func CreatesPullRequestArgv(argv []string) bool {
	if len(argv) == 0 {
		return false
	}
	switch filepath.Base(argv[0]) {
	case "lazybox":
		if len(argv) > 3 && argv[1] == "run" && argv[2] == "--" {
			return CreatesPullRequestArgv(argv[3:])
		}
		return false
	case "sh", "bash", "zsh":
		return len(argv) == 3 && (argv[1] == "-c" || argv[1] == "-lc") && createsPullRequestCommand(argv[2])
	case "gh":
		if len(argv) < 3 || argv[1] != "pr" || argv[2] != "create" {
			return false
		}
	case "hub":
		if len(argv) < 2 || argv[1] != "pull-request" {
			return false
		}
	case "glab":
		if len(argv) < 3 || argv[1] != "mr" || argv[2] != "create" {
			return false
		}
	default:
		return false
	}
	for _, arg := range argv[1:] {
		flag, _, _ := strings.Cut(arg, "=")
		if slices.Contains([]string{"--help", "--dry-run", "--web", "--browse", "--noop", "-h", "-w", "-o"}, flag) {
			return false
		}
	}
	return true
}

// Only accept a literal final create command. A preceding push, directory
// change or heredoc to a body file is common. More complex shell programs
// can exit successfully without executing the create and require an explicit link.
func createsPullRequestCommand(command string) bool {
	file, err := syntax.NewParser().Parse(strings.NewReader(command), "")
	if err != nil || len(file.Stmts) == 0 {
		return false
	}
	var valid func(*syntax.Stmt, bool) bool
	valid = func(stmt *syntax.Stmt, final bool) bool {
		if stmt.Negated || stmt.Background || stmt.Coprocess || stmt.Disown {
			return false
		}
		if binary, ok := stmt.Cmd.(*syntax.BinaryCmd); ok {
			return binary.Op == syntax.AndStmt && valid(binary.X, false) && valid(binary.Y, final)
		}
		call, ok := stmt.Cmd.(*syntax.CallExpr)
		if !ok || len(call.Args) == 0 || len(call.Assigns) != 0 {
			return false
		}
		argv := make([]string, 0, len(call.Args))
		for _, word := range call.Args {
			var arg strings.Builder
			for _, part := range word.Parts {
				switch part := part.(type) {
				case *syntax.Lit:
					arg.WriteString(part.Value)
				case *syntax.SglQuoted:
					arg.WriteString(part.Value)
				case *syntax.DblQuoted:
					for _, nested := range part.Parts {
						lit, ok := nested.(*syntax.Lit)
						if !ok {
							return false
						}
						arg.WriteString(lit.Value)
					}
				default:
					return false
				}
			}
			argv = append(argv, arg.String())
		}
		if final {
			return len(stmt.Redirs) == 0 && CreatesPullRequestArgv(argv)
		}
		switch argv[0] {
		case "git":
			return len(argv) > 1 && argv[1] == "push"
		case "cd":
			return len(argv) == 2
		case "cat":
			return len(argv) == 1 && len(stmt.Redirs) == 2 && slices.ContainsFunc(stmt.Redirs, func(r *syntax.Redirect) bool { return r.Hdoc != nil })
		}
		return false
	}
	for i, stmt := range file.Stmts {
		if !valid(stmt, i == len(file.Stmts)-1) {
			return false
		}
	}
	return true
}

// ToolCreatesPullRequest reads only a recognized tool's executable input.
// Descriptions, prompts and other arbitrary text never identify a delivery.
func ToolCreatesPullRequest(name string, input []byte) bool {
	switch name {
	case "create_pull_request", "mcp__github__create_pull_request":
		return true
	case "Shell command", "Bash", "exec_command":
		var command string
		if json.Unmarshal(input, &command) != nil {
			var args struct{ Command, Cmd string }
			if json.Unmarshal(input, &args) != nil {
				return false
			}
			command = args.Command
			if command == "" {
				command = args.Cmd
			}
		}
		return createsPullRequestCommand(command)
	}
	return false
}

// Found is a URL in output, and whether the output's command created it.
type Found struct {
	Resolved
	Role string
}

// maxFound bounds what one output contributes: a listing of many pull
// requests is not a list of things the workspace did.
const maxFound = 5

// A GitHub create tool returns the object directly or wrapped in a single
// MCP text block. Only its PR URL field identifies the created object.
func createdResultURL(output string) string {
	type block struct{ Type, Text string }
	var object struct {
		HTMLURL string  `json:"html_url"`
		URL     string  `json:"url"`
		Content []block `json:"content"`
	}
	var blocks []block
	switch {
	case json.Unmarshal([]byte(output), &object) == nil:
		if object.HTMLURL != "" {
			return object.HTMLURL
		}
		if object.URL != "" {
			return object.URL
		}
		blocks = object.Content
	case json.Unmarshal([]byte(output), &blocks) == nil:
	default:
		// The CLI prints progress and then the new URL on its own final line.
		return strings.TrimSpace(output[strings.LastIndexByte(output, '\n')+1:])
	}
	if len(blocks) != 1 || blocks[0].Type != "text" {
		return ""
	}
	var pr struct {
		HTMLURL string `json:"html_url"`
		URL     string `json:"url"`
	}
	if json.Unmarshal([]byte(blocks[0].Text), &pr) != nil {
		return strings.TrimSpace(blocks[0].Text)
	}
	if pr.HTMLURL != "" {
		return pr.HTMLURL
	}
	return pr.URL
}

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
		result := createdResultURL(strings.TrimSpace(output))
		for i, f := range all {
			if f.Kind == KindPullRequest && result == f.URL {
				delivered = i
				break
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
