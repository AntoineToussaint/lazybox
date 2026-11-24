package github

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
	"time"
)

func TestDeviceAndRefreshRequestsDoNotRequireAppSecret(t *testing.T) {
	var calls []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if err := r.ParseForm(); err != nil {
			t.Error(err)
		}
		calls = append(calls, r.URL.Path)
		if r.Method != "POST" || r.Form.Get("client_id") != "Iv.test" || r.Form.Get("client_secret") != "" {
			t.Errorf("unexpected auth request")
		}
		if r.Header.Get("Authorization") != "" {
			t.Error("login request contains unrelated auth")
		}
		switch r.URL.Path {
		case "/login/device/code":
			if r.Form.Get("scope") != "offline_access" {
				t.Error("unexpected scope")
			}
			json.NewEncoder(w).Encode(Device{DeviceCode: "private-device", UserCode: "ABCD-EFGH", VerificationURI: "https://github.com/login/device", ExpiresIn: 900, Interval: 5})
		case "/login/oauth/access_token":
			if r.Form.Get("grant_type") == "refresh_token" {
				if r.Form.Get("refresh_token") != "refresh-test" {
					t.Error("missing refresh")
				}
			} else if r.Form.Get("device_code") != "private-device" {
				t.Error("missing device")
			}
			json.NewEncoder(w).Encode(Token{AccessToken: "ghu_fake", RefreshToken: "ghr_fake", ExpiresIn: 28800, RefreshExpiresIn: 100000})
		default:
			t.Error("unexpected path")
		}
	}))
	defer server.Close()
	client := New()
	client.loginURL = server.URL
	d, err := client.Begin(context.Background(), "Iv.test")
	if err != nil || d.UserCode != "ABCD-EFGH" {
		t.Fatalf("device: %v", err)
	}
	tok, err := client.Exchange(context.Background(), "Iv.test", d.DeviceCode)
	if err != nil || tok.AccessToken != "ghu_fake" {
		t.Fatalf("exchange: %v", err)
	}
	if _, err = client.Refresh(context.Background(), "Iv.test", "refresh-test"); err != nil {
		t.Fatal(err)
	}
	if len(calls) != 3 {
		t.Fatal(calls)
	}
}
func TestAPIMappingPaginationAndErrors(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer fixture" || r.Header.Get("X-GitHub-Api-Version") == "" {
			t.Error("missing request authority")
		}
		switch r.URL.Path {
		case "/user":
			w.Write([]byte(`{"id":4,"login":"octocat"}`))
		case "/user/installations":
			w.Header().Set("Link", `<https://api.github.com/user/installations?page=3&per_page=100>; rel="next"`)
			if r.URL.Query().Get("page") != "2" {
				t.Error("missing page")
			}
			w.Write([]byte(`{"total_count":250,"installations":[{"id":9,"account":{"login":"team"},"repository_selection":"selected"}]}`))
		case "/user/installations/9/repositories":
			w.Write([]byte(`{"total_count":1,"repositories":[{"id":22,"name":"api","full_name":"team/api","private":true,"default_branch":"main"}]}`))
		case "/repositories/22":
			w.Write([]byte(`{"id":22,"name":"api","full_name":"team/api"}`))
		case "/repos/team/api/issues":
			if r.URL.Query().Get("state") != "open" {
				t.Error("expected open items")
			}
			w.Write([]byte(`[{"id":31,"number":1,"title":"Issue","user":{"login":"author"}},{"id":32,"number":2,"title":"PR","pull_request":{"url":"https://api.github.com/repos/team/api/pulls/2"}}]`))
		case "/repositories/401":
			w.WriteHeader(401)
			w.Write([]byte("secret-body"))
		case "/repositories/403":
			w.WriteHeader(403)
		case "/repositories/500":
			w.WriteHeader(500)
			w.Write([]byte("secret-body"))
		case "/repositories/302":
			http.Redirect(w, r, "https://evil.invalid", 302)
		}
	}))
	defer server.Close()
	c := New()
	c.apiURL = server.URL
	ctx := context.Background()
	u, err := c.User(ctx, "fixture")
	if err != nil || u.Login != "octocat" {
		t.Fatal(u, err)
	}
	installs, more, err := c.Installations(ctx, "fixture", 2)
	if err != nil || !more || installs[0].ID != 9 {
		t.Fatal(installs, more, err)
	}
	repos, more, err := c.Repositories(ctx, "fixture", 9, 1)
	if err != nil || more || !repos[0].Private {
		t.Fatal(repos, more, err)
	}
	repo, err := c.Repository(ctx, "fixture", 22)
	if err != nil || repo.ID != 22 {
		t.Fatal(repo, err)
	}
	items, more, err := c.Items(ctx, "fixture", "team/api", 1)
	if err != nil || more || items[0].PullRequest != nil || items[1].PullRequest == nil {
		t.Fatal(items, err)
	}
	for _, id := range []int64{401, 403, 500, 302} {
		_, err = c.Repository(ctx, "fixture", id)
		if err == nil || strings.Contains(err.Error(), "secret-body") {
			t.Fatal(err)
		}
		if id == 401 && !errors.Is(err, ErrUnauthorized) {
			t.Fatal(err)
		}
	}
	if _, _, err = c.Repositories(ctx, "fixture", 0, 1); err == nil {
		t.Fatal("accepted absent installation")
	}
	if _, _, err = c.Items(ctx, "fixture", "bad", 1); err == nil {
		t.Fatal("accepted invalid repo")
	}
	if _, _, err = c.Installations(ctx, "fixture", 1001); err == nil {
		t.Fatal("accepted unbounded page")
	}
}

func TestPersonalDiscoveryUsesUserReposAndLinkPagination(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/user/repos" || r.URL.Query().Get("affiliation") != "owner,collaborator,organization_member" || r.URL.Query().Get("per_page") != "100" {
			t.Error("wrong discovery request", r.URL.Path)
		}
		if r.URL.Query().Get("page") == "1" {
			if r.Header.Get("Authorization") != "Bearer ghp_first" {
				t.Error("incorrect first credential")
			}
			w.Header().Set("Link", `<https://api.github.com/user/repos?page=2>; rel="next"`)
			w.Write([]byte(`[{"id":11,"name":"api","full_name":"another-org/api","private":true,"clone_url":"https://github.com/another-org/api.git","html_url":"https://github.com/another-org/api","default_branch":"main"}]`))
		} else {
			if r.Header.Get("Authorization") != "Bearer github_pat_second" {
				t.Error("reused another credential")
			}
			w.Write([]byte(`[]`))
		}
	}))
	defer server.Close()
	c := New()
	c.apiURL = server.URL
	repos, more, err := c.UserRepositories(context.Background(), "ghp_first", 1)
	if err != nil || !more || len(repos) != 1 || repos[0].FullName != "another-org/api" || !repos[0].Private {
		t.Fatal(repos, more, err)
	}
	_, more, err = c.UserRepositories(context.Background(), "github_pat_second", 2)
	if err != nil || more {
		t.Fatal(more, err)
	}
	if _, _, err := c.UserRepositories(context.Background(), "unused", 0); err == nil {
		t.Fatal("accepted unbounded page")
	}
}

func TestSDKRejectsOversizedBodiesAndRedactsErrors(t *testing.T) {
	for _, response := range []string{strings.Repeat("x", (4<<20)+1), `{"secret-body":invalid-json}`} {
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(response)) }))
		c := New()
		c.apiURL = server.URL
		_, err := c.User(context.Background(), "ghp_do_not_echo")
		server.Close()
		if err == nil || strings.Contains(err.Error(), "secret-body") || strings.Contains(err.Error(), "ghp_") {
			t.Fatal("unbounded or unsafe error", err)
		}
	}
}

// A rate limit is not a refusal: the token still works, and the error says
// when to try again instead of sending the person to reconnect it. Each of
// GitHub's documented signals is told apart, on the REST API, on sign-in
// and on a job's logs.
func TestRateLimitsAreToldApartFromRefusals(t *testing.T) {
	reset := time.Now().Add(17 * time.Minute).Truncate(time.Second)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/repositories/1": // primary: x-ratelimit-remaining 0, until x-ratelimit-reset
			w.Header().Set("X-RateLimit-Remaining", "0")
			w.Header().Set("X-RateLimit-Reset", strconv.FormatInt(reset.Unix(), 10))
			w.WriteHeader(403)
			w.Write([]byte(`{"message":"API rate limit exceeded for user ID 1."}`))
		case "/repositories/2": // secondary, with retry-after
			w.Header().Set("Retry-After", "120")
			w.WriteHeader(403)
			w.Write([]byte(`{"message":"You have exceeded a secondary rate limit.","documentation_url":"https://docs.github.com/rest/overview/rate-limits-for-the-rest-api#about-secondary-rate-limits"}`))
		case "/repositories/3": // secondary, its message only
			w.WriteHeader(403)
			w.Write([]byte(`{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}`))
		case "/repositories/4": // 429 with retry-after
			w.Header().Set("Retry-After", "30")
			w.WriteHeader(429)
		case "/repositories/5": // a refusal
			w.WriteHeader(403)
			w.Write([]byte(`{"message":"Resource not accessible by personal access token"}`))
		case "/login/device/code":
			w.Header().Set("Retry-After", "90")
			w.WriteHeader(429)
		case "/repos/team/api/actions/jobs/9/logs":
			w.Header().Set("Retry-After", "60")
			w.WriteHeader(403)
			w.Write([]byte(`{"message":"You have exceeded a secondary rate limit."}`))
		}
	}))
	defer server.Close()
	c := NewAt(server.URL)
	c.loginURL = server.URL
	ctx := context.Background()
	limited := func(what string, err error, after time.Duration) {
		t.Helper()
		var limit *RateLimitError
		if !errors.As(err, &limit) || errors.Is(err, ErrForbidden) {
			t.Fatalf("%s: not told apart: %v", what, err)
		}
		if wait := time.Until(limit.Reset); wait < after-5*time.Second || wait > after+5*time.Second {
			t.Fatalf("%s: retry in %s, want %s", what, wait, after)
		}
		text := err.Error()
		if !strings.Contains(text, limit.Reset.UTC().Format("15:04")+" UTC") || !strings.Contains(text, "still works") || strings.Contains(text, "permission") {
			t.Fatalf("%s: %q", what, text)
		}
	}
	_, err := c.Repository(ctx, "fixture", 1)
	limited("primary", err, time.Until(reset))
	if !strings.Contains(err.Error(), "in about 17 minutes") {
		t.Fatalf("primary: %q", err)
	}
	_, err = c.Repository(ctx, "fixture", 2)
	limited("secondary with retry-after", err, 2*time.Minute)
	_, err = c.Repository(ctx, "fixture", 3)
	limited("secondary by its message", err, secondaryWait)
	_, err = c.Repository(ctx, "fixture", 4)
	limited("429", err, 30*time.Second)
	if _, err = c.Repository(ctx, "fixture", 5); !errors.Is(err, ErrForbidden) || strings.Contains(err.Error(), "rate limit") {
		t.Fatalf("a refusal is a refusal: %v", err)
	}
	_, err = c.Begin(ctx, "Iv.test")
	limited("sign-in", err, 90*time.Second)
	_, err = c.JobLog(ctx, "fixture", "team/api", 9)
	limited("job logs", err, time.Minute)
}
