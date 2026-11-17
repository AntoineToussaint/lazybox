package githubtest

import (
	"fmt"
	"net/http"
	"sort"
	"strconv"
	"strings"
)

// Issue is an issue (or, with PullRequest, a pull request as the issues API
// shows it) in the fake. Comments are oldest first.
type Issue struct {
	Number      int
	Title, Body string
	State       string // open (default) or closed
	StateReason string
	PullRequest bool
	Merged      bool
	Author      string
	Assignees   []string
	Labels      []string
	Comments    []IssueComment
	UpdatedAt   string // RFC 3339
}

type IssueComment struct {
	Author, Body, CreatedAt string
}

// IssueStore holds issues by repository full name. Login is who @me is.
type IssueStore struct {
	Login    string
	ByRepo   map[string][]*Issue
	Searches []string // every search query, for assertions
}

// AddIssue adds an issue to owner/name.
func (s *Server) AddIssue(fullName string, issue Issue) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.Issues.ByRepo == nil {
		s.Issues.ByRepo = map[string][]*Issue{}
	}
	if issue.State == "" {
		issue.State = "open"
	}
	copy := issue
	s.Issues.ByRepo[fullName] = append(s.Issues.ByRepo[fullName], &copy)
}

func (s *Server) issueWire(full string, issue *Issue) map[string]any {
	kind := "issues"
	if issue.PullRequest {
		kind = "pull"
	}
	html := "https://github.com/" + full + "/" + kind + "/" + strconv.Itoa(issue.Number)
	assignees := []map[string]any{}
	for _, login := range issue.Assignees {
		assignees = append(assignees, map[string]any{"login": login})
	}
	labels := []map[string]any{}
	for _, name := range issue.Labels {
		labels = append(labels, map[string]any{"name": name})
	}
	author := issue.Author
	if author == "" {
		author = "fixture"
	}
	out := map[string]any{
		"number": issue.Number, "title": issue.Title, "body": issue.Body, "state": issue.State, "html_url": html,
		"repository_url": "https://api.github.com/repos/" + full, "user": map[string]any{"login": author},
		"assignees": assignees, "labels": labels, "comments": len(issue.Comments),
	}
	if issue.StateReason != "" {
		out["state_reason"] = issue.StateReason
	}
	if issue.UpdatedAt != "" {
		out["updated_at"] = issue.UpdatedAt
	}
	if issue.PullRequest {
		links := map[string]any{"url": "https://api.github.com/repos/" + full + "/pulls/" + strconv.Itoa(issue.Number)}
		if issue.Merged {
			links["merged_at"] = "2026-11-01T00:00:00Z"
		}
		out["pull_request"] = links
	}
	return out
}

// serveIssues answers the issue reads and the issue search; false leaves the
// request to the pull-request routes.
func (s *Server) serveIssues(w http.ResponseWriter, r *http.Request) bool {
	parts := strings.Split(strings.Trim(r.URL.Path, "/"), "/")
	if r.Method != "GET" {
		return false
	}
	if len(parts) == 2 && parts[0] == "search" && parts[1] == "issues" {
		s.search(w, r)
		return true
	}
	if len(parts) < 5 || parts[0] != "repos" || parts[3] != "issues" {
		return false
	}
	full := parts[1] + "/" + parts[2]
	number, _ := strconv.Atoi(parts[4])
	var issue *Issue
	for _, candidate := range s.Issues.ByRepo[full] {
		if candidate.Number == number {
			issue = candidate
		}
	}
	if issue == nil {
		write(w, 404, map[string]any{"message": "Not Found"})
		return true
	}
	switch {
	case len(parts) == 5:
		write(w, 200, s.issueWire(full, issue))
	case len(parts) == 6 && parts[5] == "comments":
		page, _ := strconv.Atoi(r.URL.Query().Get("page"))
		per, _ := strconv.Atoi(r.URL.Query().Get("per_page"))
		if page < 1 {
			page = 1
		}
		if per < 1 {
			per = 30
		}
		last := (len(issue.Comments) + per - 1) / per
		start, end := (page-1)*per, page*per
		start, end = min(start, len(issue.Comments)), min(end, len(issue.Comments))
		out := []map[string]any{}
		for i, c := range issue.Comments[start:end] {
			out = append(out, map[string]any{"user": map[string]any{"login": c.Author}, "body": c.Body, "created_at": c.CreatedAt,
				"html_url": fmt.Sprintf("https://github.com/%s/issues/%d#issuecomment-%d", full, number, start+i+1)})
		}
		if page < last {
			link := func(p int) string {
				return fmt.Sprintf("<%s%s?page=%d&per_page=%d>", "http://"+r.Host, r.URL.Path, p, per)
			}
			w.Header().Set("Link", link(page+1)+`; rel="next", `+link(last)+`; rel="last"`)
		}
		write(w, 200, out)
	default:
		write(w, 404, map[string]any{"message": "Not Found"})
	}
	return true
}

// search understands the qualifiers the Runner sends (is:, assignee:, involves:,
// author:, repo:, archived:) and matches the remaining words in the title or body.
func (s *Server) search(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query().Get("q")
	s.Issues.Searches = append(s.Issues.Searches, q)
	me := s.Issues.Login
	if me == "" {
		me = "fixture"
	}
	type hit struct {
		full  string
		issue *Issue
	}
	hits := []hit{}
	for full, issues := range s.Issues.ByRepo {
	next:
		for _, issue := range issues {
			for _, term := range strings.Fields(q) {
				key, value, qualified := strings.Cut(term, ":")
				if value == "@me" {
					value = me
				}
				switch {
				case qualified && key == "is" && value == "issue" && issue.PullRequest,
					qualified && key == "is" && value == "pr" && !issue.PullRequest,
					qualified && key == "is" && value == "open" && issue.State != "open",
					qualified && key == "is" && value == "closed" && issue.State == "open",
					qualified && key == "assignee" && !contains(issue.Assignees, value),
					qualified && key == "author" && issue.Author != value,
					qualified && key == "involves" && issue.Author != value && !contains(issue.Assignees, value) && !commented(issue, value),
					qualified && key == "repo" && !strings.EqualFold(full, value):
					continue next
				case qualified && (key == "is" || key == "assignee" || key == "author" || key == "involves" || key == "repo" || key == "archived" || key == "sort" || key == "in"):
				default:
					text := strings.ToLower(issue.Title + "\n" + issue.Body)
					if !strings.Contains(text, strings.ToLower(term)) {
						continue next
					}
				}
			}
			hits = append(hits, hit{full, issue})
		}
	}
	sort.Slice(hits, func(i, j int) bool {
		if hits[i].issue.UpdatedAt != hits[j].issue.UpdatedAt {
			return hits[i].issue.UpdatedAt > hits[j].issue.UpdatedAt
		}
		return hits[i].full+strconv.Itoa(hits[i].issue.Number) < hits[j].full+strconv.Itoa(hits[j].issue.Number)
	})
	items := []map[string]any{}
	for _, h := range hits {
		items = append(items, s.issueWire(h.full, h.issue))
	}
	write(w, 200, map[string]any{"total_count": len(items), "incomplete_results": false, "items": items})
}

func contains(values []string, value string) bool {
	for _, v := range values {
		if strings.EqualFold(v, value) {
			return true
		}
	}
	return false
}

func commented(issue *Issue, login string) bool {
	for _, c := range issue.Comments {
		if strings.EqualFold(c.Author, login) {
			return true
		}
	}
	return false
}
