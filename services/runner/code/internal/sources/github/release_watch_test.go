package github

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestReleaseWatchLatestFailureAndEligibleApprovals(t *testing.T) {
	responses := map[string]string{
		"/repos/team/project/deployments": `[
			{"id":3,"environment":"production"},{"id":2,"environment":"production"},{"id":1,"environment":"staging"}]`,
		"/repos/team/project/deployments/3/statuses": `[{"state":"failure","log_url":"https://github.com/team/project/actions/runs/37/job/9"}]`,
		"/repos/team/project/deployments/1/statuses": `[{"state":"success"}]`,
		"/repos/team/project/actions/runs":           `{"workflow_runs":[{"id":7}]}`,
		"/repos/team/project/actions/runs/7/pending_deployments": `[
			{"environment":{"id":4,"name":"production"},"current_user_can_approve":true},
			{"environment":{"id":5,"name":"limited"},"current_user_can_approve":false}]`,
	}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/repos/team/project/actions/runs" && r.URL.Query().Get("status") != "waiting" {
			t.Errorf("expected only waiting runs: %s", r.URL)
		}
		if value, ok := responses[r.URL.Path]; ok {
			fmt.Fprint(w, value)
			return
		}
		t.Errorf("unexpected GitHub request: %s", r.URL)
		http.NotFound(w, r)
	}))
	defer server.Close()
	rows, err := NewAt(server.URL).ReleaseWatch(context.Background(), "fixture-token", "team/project")
	if err != nil || len(rows) != 2 {
		t.Fatalf("releases: %+v, %v", rows, err)
	}
	if rows[0].Reason != "deployment_failed" || rows[0].URL != "https://github.com/team/project/actions/runs/37/job/9" ||
		rows[1].Reason != "deployment_approval" || rows[1].URL != "https://github.com/team/project/actions/runs/7" {
		t.Fatalf("unexpected release items: %+v", rows)
	}
}
