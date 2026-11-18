package github

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"slices"
)

type ReleaseNotice struct {
	ID, Environment, Reason, URL, UpdatedAt string
}

var repoName = regexp.MustCompile(`^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`)

// ReleaseWatch reads a bounded snapshot: latest deployment in each recently
// used environment, and runs currently waiting for an approval this user can give.
func (c *Client) ReleaseWatch(ctx context.Context, token, repository string) ([]ReleaseNotice, error) {
	if !repoName.MatchString(repository) {
		return nil, errors.New("invalid GitHub repository")
	}
	base := c.apiURL + "/repos/" + repository
	page := "https://github.com/" + repository
	var (
		items       []ReleaseNotice
		deployments []struct {
			ID          int64
			Environment string
		}
		workflows struct {
			Runs []struct {
				ID        int64
				UpdatedAt string `json:"updated_at"`
			} `json:"workflow_runs"`
		}
	)
	deploymentErr := c.request(ctx, "GET", base+"/deployments?per_page=5", token, nil, &deployments)
	if deploymentErr == nil {
		environments := map[string]bool{}
		for _, deployment := range deployments {
			if deployment.ID <= 0 || environments[deployment.Environment] {
				continue
			}
			environments[deployment.Environment] = true
			var statuses []struct {
				State     string
				LogURL    string `json:"log_url"`
				UpdatedAt string `json:"updated_at"`
			}
			err := c.request(ctx, "GET", fmt.Sprintf("%s/deployments/%d/statuses?per_page=1", base, deployment.ID), token, nil, &statuses)
			if err != nil {
				deploymentErr = err
				continue
			}
			if len(statuses) == 0 || !slices.Contains([]string{"failure", "error"}, statuses[0].State) {
				continue
			}
			status := statuses[0]
			link := page + "/deployments"
			if ok, _ := regexp.MatchString("^"+regexp.QuoteMeta(page)+`/actions/runs/[0-9]+(?:/job/[0-9]+)?$`, status.LogURL); ok {
				link = status.LogURL
			}
			items = append(items, ReleaseNotice{fmt.Sprintf("deployment:%d", deployment.ID), deployment.Environment, "deployment_failed", link, status.UpdatedAt})
		}
	}
	approvalErr := c.request(ctx, "GET", base+"/actions/runs?status=waiting&per_page=5", token, nil, &workflows)
	if approvalErr == nil {
		for _, run := range workflows.Runs {
			if run.ID <= 0 {
				continue
			}
			var pending []struct {
				Environment struct {
					ID   int64
					Name string
				}
				CanApprove bool `json:"current_user_can_approve"`
			}
			if err := c.request(ctx, "GET", fmt.Sprintf("%s/actions/runs/%d/pending_deployments", base, run.ID), token, nil, &pending); err != nil {
				approvalErr = err
				continue
			}
			for _, environment := range pending {
				if environment.CanApprove {
					items = append(items, ReleaseNotice{fmt.Sprintf("approval:%d:%d", run.ID, environment.Environment.ID), environment.Environment.Name,
						"deployment_approval", fmt.Sprintf("%s/actions/runs/%d", page, run.ID), run.UpdatedAt})
				}
			}
		}
	}
	return items, errors.Join(deploymentErr, approvalErr)
}
