// Package githubtest is an in-memory GitHub REST API for PR-loop and
// tracking tests: pull requests, reviews and their comments, check runs,
// commit statuses, Actions job logs and merges, over httptest. Reads honour
// If-None-Match as GitHub does (304, counted in NotModified), and RateLimited
// answers as a primary rate limit. It never contacts GitHub.
package githubtest

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

type Check struct {
	Name, Status, Conclusion, URL string
	// ID is the check run's id: an Actions job's, for Jobs.
	ID int64
}

// Comment is an inline review comment.
type Comment struct {
	ID, InReplyTo   int64
	Author, Body    string
	Path            string
	Line, StartLine int
	// Side is RIGHT or LEFT; Hunk the diff hunk GitHub shows above it.
	Side, Hunk string
	// Outdated drops the current line, as GitHub does once the lines change.
	Outdated bool
}
type Status struct {
	Context, State, URL string
}
type PR struct {
	Number                   int
	Title, Body, Head, Base  string
	HeadSHA                  string
	Draft, Merged            bool
	State                    string
	MergeableState           string
	MergeMethod, MergeCommit string
	// BranchUpdates counts update-branch requests GitHub accepted.
	BranchUpdates int
	pendingUpdate int
	Reviews       []string // "login:STATE" or "login:STATE:body"
	Requested     int
	Comments      []Comment
	CreatedAt     time.Time
}

// Server is a fake GitHub for one token. Set MergeRefusal to make merges fail
// as branch protection does (HTTP 405 with GitHub's message).
type Server struct {
	*httptest.Server
	Token string
	// UnauthorizedPath answers 401 to requests whose path ends with it, as
	// GitHub does to a write a token may no longer make; others succeed.
	UnauthorizedPath string
	// UpdateLag is how many reads of a pull request pass before an accepted
	// branch update shows (GitHub updates in the background); 0 is at once.
	UpdateLag    int
	mu           sync.Mutex
	Repos        map[string]string // full name → default branch
	Heads        map[string]string // "owner/name:branch" → pushed head
	Pulls        map[string][]*PR  // full name → pull requests
	Checks       map[string][]Check
	Statuses     map[string][]Status
	MergeRefusal string
	Requests     []string
	Unauthorized int
	ReadOnly     bool
	// Issues and their comments, for the start-from-an-issue reads (issues.go).
	Issues IssueStore
	// Jobs are Actions job logs by job id; LogsForbidden refuses them as a
	// token without Actions read access is refused.
	Jobs          map[int64]string
	LogsForbidden bool
	// NotModified counts conditional reads answered 304.
	NotModified int
	// RateLimited answers every API read as a spent primary rate limit.
	RateLimited bool
	// Associations are review authors' relation to the repository by login
	// (author_association); MEMBER when not named.
	Associations map[string]string
}

// association is a login's author_association. Caller holds mu.
func (s *Server) association(login string) string {
	if a := s.Associations[login]; a != "" {
		return a
	}
	return "MEMBER"
}

func New(t *testing.T, token string) *Server {
	s := &Server{Token: token, Repos: map[string]string{}, Heads: map[string]string{}, Pulls: map[string][]*PR{}, Checks: map[string][]Check{}, Statuses: map[string][]Status{}, Jobs: map[int64]string{}}
	s.Server = httptest.NewServer(http.HandlerFunc(s.serve))
	t.Cleanup(s.Close)
	return s
}

// Pull returns a pull request by number for assertions.
func (s *Server) Pull(fullName string, number int) *PR {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, pr := range s.Pulls[fullName] {
		if pr.Number == number {
			copy := *pr
			return &copy
		}
	}
	return nil
}

// SetHead records a push: the branch, and its open pull request, now point at sha.
func (s *Server) SetHead(fullName, branch, sha string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.Heads[fullName+":"+branch] = sha
	for _, pr := range s.Pulls[fullName] {
		if pr.Head == branch && pr.State == "open" {
			pr.HeadSHA = sha
		}
	}
}

func (s *Server) Update(f func(*Server)) { s.mu.Lock(); defer s.mu.Unlock(); f(s) }

func write(w http.ResponseWriter, code int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(value)
}

// read answers a GET with an ETag of its body, and 304 when the client
// already has it.
func (s *Server) read(w http.ResponseWriter, r *http.Request, value any) {
	var body bytes.Buffer
	_ = json.NewEncoder(&body).Encode(value)
	sum := sha256.Sum256(body.Bytes())
	tag := `W/"` + hex.EncodeToString(sum[:8]) + `"`
	w.Header().Set("ETag", tag)
	if r.Header.Get("If-None-Match") == tag {
		s.NotModified++
		w.WriteHeader(http.StatusNotModified)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(body.Bytes())
}

func (s *Server) wire(full string, pr *PR) map[string]any {
	state := pr.State
	if pr.Merged {
		state = "closed"
	}
	reviewers := []map[string]any{}
	for i := 0; i < pr.Requested; i++ {
		reviewers = append(reviewers, map[string]any{"login": "reviewer" + strconv.Itoa(i)})
	}
	created := pr.CreatedAt
	if created.IsZero() {
		created = time.Date(2026, 11, 1, 9, 0, 0, 0, time.UTC)
	}
	// GitHub moves updated_at when a review is submitted, so a pull request
	// whose reviews changed is never "not modified".
	updated := created.Add(time.Duration(len(pr.Reviews)+len(pr.Comments)) * time.Minute)
	times := map[string]any{"created_at": created.Format(time.RFC3339), "updated_at": updated.Format(time.RFC3339)}
	if pr.Merged {
		times["merged_at"], times["closed_at"] = updated.Format(time.RFC3339), updated.Format(time.RFC3339)
	} else if pr.State == "closed" {
		times["closed_at"] = updated.Format(time.RFC3339)
	}
	wire := map[string]any{
		"number": pr.Number, "title": pr.Title, "body": pr.Body, "state": state, "draft": pr.Draft, "merged": pr.Merged,
		"html_url": "https://github.com/" + full + "/pull/" + strconv.Itoa(pr.Number), "mergeable": pr.MergeableState == "clean" || pr.MergeableState == "unstable" || pr.MergeableState == "behind",
		"mergeable_state": pr.MergeableState, "merge_commit_sha": pr.MergeCommit, "user": map[string]any{"login": "fixture"},
		"head": map[string]any{"ref": pr.Head, "sha": pr.HeadSHA}, "base": map[string]any{"ref": pr.Base}, "requested_reviewers": reviewers,
	}
	for k, v := range times {
		wire[k] = v
	}
	return wire
}

func (s *Server) serve(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.Requests = append(s.Requests, r.Method+" "+r.URL.Path)
	parts := strings.Split(strings.Trim(r.URL.Path, "/"), "/")
	// A job log's storage: a signed link, read without the credential.
	if len(parts) == 2 && parts[0] == "blobs" {
		id, _ := strconv.ParseInt(parts[1], 10, 64)
		if text, ok := s.Jobs[id]; ok {
			w.WriteHeader(http.StatusOK)
			_, _ = w.Write([]byte(text))
			return
		}
		write(w, 404, map[string]any{"message": "Not Found"})
		return
	}
	if r.Header.Get("Authorization") != "Bearer "+s.Token || (s.UnauthorizedPath != "" && strings.HasSuffix(r.URL.Path, s.UnauthorizedPath)) {
		s.Unauthorized++
		write(w, 401, map[string]any{"message": "Bad credentials"})
		return
	}
	if s.RateLimited {
		w.Header().Set("X-RateLimit-Remaining", "0")
		w.Header().Set("X-RateLimit-Reset", strconv.FormatInt(time.Now().Add(10*time.Minute).Unix(), 10))
		write(w, 403, map[string]any{"message": "API rate limit exceeded"})
		return
	}
	if s.serveIssues(w, r) {
		return
	}
	if len(parts) < 3 || parts[0] != "repos" {
		write(w, 404, map[string]any{"message": "Not Found"})
		return
	}
	full := parts[1] + "/" + parts[2]
	base, ok := s.Repos[full]
	if !ok {
		write(w, 404, map[string]any{"message": "Not Found"})
		return
	}
	rest := parts[3:]
	switch {
	case len(rest) == 0 && r.Method == "GET":
		write(w, 200, map[string]any{"full_name": full, "default_branch": base})
	case len(rest) == 1 && rest[0] == "pulls" && r.Method == "GET":
		head := r.URL.Query().Get("head")
		out := []map[string]any{}
		pulls := s.Pulls[full]
		for i := len(pulls) - 1; i >= 0; i-- {
			if parts[1]+":"+pulls[i].Head == head {
				out = append(out, s.wire(full, pulls[i]))
			}
		}
		s.read(w, r, out)
	case len(rest) == 1 && rest[0] == "pulls" && r.Method == "POST":
		if s.ReadOnly {
			write(w, 403, map[string]any{"message": "Resource not accessible by integration"})
			return
		}
		var body struct {
			Title, Head, Base, Body string
			Draft                   bool
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		for _, pr := range s.Pulls[full] {
			if pr.Head == body.Head && pr.State == "open" {
				write(w, 422, map[string]any{"message": "Validation Failed", "errors": []map[string]any{{"resource": "PullRequest", "code": "custom", "message": "A pull request already exists for " + parts[1] + ":" + body.Head + "."}}})
				return
			}
		}
		sha := s.Heads[full+":"+body.Head]
		if sha == "" {
			write(w, 422, map[string]any{"message": "Validation Failed", "errors": []map[string]any{{"resource": "PullRequest", "field": "head", "code": "invalid"}}})
			return
		}
		pr := &PR{Number: len(s.Pulls[full]) + 1, Title: body.Title, Body: body.Body, Head: body.Head, Base: body.Base, Draft: body.Draft, State: "open", MergeableState: "unknown", HeadSHA: sha}
		s.Pulls[full] = append(s.Pulls[full], pr)
		write(w, 201, s.wire(full, pr))
	case len(rest) >= 2 && rest[0] == "pulls":
		number, _ := strconv.Atoi(rest[1])
		var pr *PR
		for _, candidate := range s.Pulls[full] {
			if candidate.Number == number {
				pr = candidate
			}
		}
		if pr == nil {
			write(w, 404, map[string]any{"message": "Not Found"})
			return
		}
		switch {
		case len(rest) == 2 && r.Method == "GET":
			// An accepted branch update lands after UpdateLag reads, as GitHub's does in the background.
			if pr.pendingUpdate > 0 {
				if pr.pendingUpdate--; pr.pendingUpdate == 0 {
					pr.HeadSHA, pr.MergeableState = pr.HeadSHA+"-updated", "clean"
				}
			}
			s.read(w, r, s.wire(full, pr))
		case len(rest) == 3 && rest[2] == "reviews" && r.Method == "GET":
			out := []map[string]any{}
			for i, review := range pr.Reviews {
				fields := strings.SplitN(review, ":", 3)
				entry := map[string]any{"id": i + 1, "user": map[string]any{"login": fields[0]}, "state": fields[1], "author_association": s.association(fields[0])}
				if len(fields) == 3 {
					entry["body"] = fields[2]
				}
				out = append(out, entry)
			}
			write(w, 200, out)
		case len(rest) == 3 && rest[2] == "comments" && r.Method == "GET":
			out := []map[string]any{}
			for _, c := range pr.Comments {
				entry := map[string]any{"id": c.ID, "user": map[string]any{"login": c.Author}, "author_association": s.association(c.Author), "body": c.Body, "path": c.Path, "side": c.Side, "diff_hunk": c.Hunk,
					"original_line": c.Line, "html_url": fmt.Sprintf("https://github.com/%s/pull/%d#discussion_r%d", full, pr.Number, c.ID), "commit_id": pr.HeadSHA}
				if !c.Outdated {
					entry["line"] = c.Line
				}
				if c.StartLine > 0 {
					entry["start_line"] = c.StartLine
				}
				if c.InReplyTo > 0 {
					entry["in_reply_to_id"] = c.InReplyTo
				}
				out = append(out, entry)
			}
			write(w, 200, out)
		case len(rest) == 3 && rest[2] == "merge" && r.Method == "PUT":
			var body struct {
				SHA         string `json:"sha"`
				MergeMethod string `json:"merge_method"`
			}
			_ = json.NewDecoder(r.Body).Decode(&body)
			if s.MergeRefusal != "" {
				write(w, 405, map[string]any{"message": s.MergeRefusal})
				return
			}
			// A repository that requires up-to-date branches refuses one behind its base.
			if pr.MergeableState == "behind" {
				write(w, 405, map[string]any{"message": "Head branch is out of date. Review and try the merge again."})
				return
			}
			if body.SHA != pr.HeadSHA {
				write(w, 409, map[string]any{"message": "Head branch was modified. Review and try the merge again."})
				return
			}
			pr.Merged, pr.State, pr.MergeMethod, pr.MergeCommit = true, "closed", body.MergeMethod, "merged-"+pr.HeadSHA
			write(w, 200, map[string]any{"merged": true, "sha": pr.MergeCommit, "message": "Pull Request successfully merged"})
		case len(rest) == 3 && rest[2] == "update-branch" && r.Method == "PUT":
			var body struct {
				ExpectedHeadSHA string `json:"expected_head_sha"`
			}
			_ = json.NewDecoder(r.Body).Decode(&body)
			if body.ExpectedHeadSHA != "" && body.ExpectedHeadSHA != pr.HeadSHA {
				write(w, 422, map[string]any{"message": "expected head sha didn't match current head ref."})
				return
			}
			// GitHub merges the base into the branch in the background: a new head, checks to run again.
			pr.BranchUpdates++
			if s.UpdateLag > 0 {
				pr.pendingUpdate = s.UpdateLag
			} else {
				pr.HeadSHA, pr.MergeableState = pr.HeadSHA+"-updated", "clean"
			}
			write(w, 202, map[string]any{"message": "Updating pull request branch.", "url": "https://github.com/" + full + "/pull/" + strconv.Itoa(pr.Number)})
		default:
			write(w, 404, map[string]any{"message": "Not Found"})
		}
	case len(rest) == 3 && rest[0] == "commits" && rest[2] == "check-runs":
		runs := []map[string]any{}
		for _, c := range s.Checks[rest[1]] {
			runs = append(runs, map[string]any{"id": c.ID, "name": c.Name, "status": c.Status, "conclusion": c.Conclusion, "html_url": c.URL})
		}
		s.read(w, r, map[string]any{"total_count": len(runs), "check_runs": runs})
	case len(rest) == 3 && rest[0] == "commits" && rest[2] == "status":
		statuses := []map[string]any{}
		for _, c := range s.Statuses[rest[1]] {
			statuses = append(statuses, map[string]any{"context": c.Context, "state": c.State, "target_url": c.URL})
		}
		s.read(w, r, map[string]any{"state": "pending", "statuses": statuses})
	case len(rest) == 4 && rest[0] == "actions" && rest[1] == "jobs" && rest[3] == "logs":
		if s.LogsForbidden {
			write(w, 403, map[string]any{"message": "Resource not accessible by personal access token"})
			return
		}
		id, _ := strconv.ParseInt(rest[2], 10, 64)
		if _, ok := s.Jobs[id]; !ok {
			write(w, 404, map[string]any{"message": "Not Found"})
			return
		}
		http.Redirect(w, r, "/blobs/"+rest[2], http.StatusFound)
	default:
		write(w, 404, map[string]any{"message": fmt.Sprintf("Not Found: %s", r.URL.Path)})
	}
}
