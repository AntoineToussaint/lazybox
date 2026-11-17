package github_test

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
)

func TestCreateIssue(t *testing.T) {
	var got map[string]any
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer ghp_fixture" {
			w.WriteHeader(401)
			return
		}
		switch r.URL.Path {
		case "/repos/team/api/issues":
			if r.Method != http.MethodPost {
				t.Errorf("method %s", r.Method)
			}
			_ = json.NewDecoder(r.Body).Decode(&got)
			w.WriteHeader(201)
			_, _ = w.Write([]byte(`{"number":7,"html_url":"https://github.com/team/api/issues/7"}`))
		case "/repos/team/locked/issues":
			w.WriteHeader(403)
			_, _ = w.Write([]byte(`{"message":"Resource not accessible by integration"}`))
		default:
			w.WriteHeader(404)
		}
	}))
	defer server.Close()
	client := github.NewAt(server.URL)
	var issues github.Issues = client
	issue, err := issues.CreateIssue(context.Background(), "ghp_fixture", "team/api", " Rollout plan ", "# Plan\nbody")
	if err != nil || issue.Number != 7 || issue.URL != "https://github.com/team/api/issues/7" {
		t.Fatal(issue, err)
	}
	if got["title"] != "Rollout plan" || got["body"] != "# Plan\nbody" {
		t.Fatalf("sent %v", got)
	}
	if _, err = client.CreateIssue(context.Background(), "ghp_fixture", "team/locked", "T", "b"); err == nil || !strings.Contains(err.Error(), "Issues write access") {
		t.Fatal(err)
	}
	if _, err = client.CreateIssue(context.Background(), "ghp_fixture", "team/api", "  ", "b"); err == nil {
		t.Fatal("accepted an empty title")
	}
	if _, err = client.CreateIssue(context.Background(), "ghp_fixture", "nope", "T", "b"); err == nil {
		t.Fatal("accepted a repository without an owner")
	}
}
