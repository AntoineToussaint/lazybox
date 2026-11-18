package github

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestMailboxUsesPersonalNotificationsAndCanonicalLinks(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet || r.URL.Path != "/notifications" || r.URL.Query().Get("all") != "" ||
			r.URL.Query().Get("page") != "1" || r.Header.Get("Authorization") != "Bearer fixture-token" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL)
		}
		fmt.Fprint(w, `[
			{"id":"one","reason":"review_requested","updated_at":"2026-11-18T17:00:00Z","repository":{"id":42,"full_name":"team/project"},"subject":{"title":"Review this","type":"PullRequest","url":"https://api.github.com/repos/team/project/pulls/5"}},
			{"id":"one","repository":{"full_name":"team/project"},"subject":{"title":"Duplicate","type":"PullRequest","url":"https://api.github.com/repos/team/project/pulls/5"}},
			{"id":"two","reason":"assign","repository":{"id":42,"full_name":"team/project"},"subject":{"title":"Fix a bug","type":"Issue","url":"https://api.github.com/repos/team/project/issues/9"}},
			{"id":"bad","repository":{"full_name":"team/project"},"subject":{"type":"Issue","url":"https://api.github.com/repos/other/project/issues/9"}}
		]`)
	}))
	defer server.Close()
	items, truncated, err := NewAt(server.URL).Mailbox(context.Background(), "fixture-token")
	if err != nil || truncated || len(items) != 2 {
		t.Fatalf("mailbox: %+v, truncated=%v, error=%v", items, truncated, err)
	}
	if items[0].URL != "https://github.com/team/project/pull/5" || items[0].Reason != "review_requested" ||
		items[1].URL != "https://github.com/team/project/issues/9" || items[1].Reason != "assign" {
		t.Fatalf("wrong notification data: %+v", items)
	}
}
