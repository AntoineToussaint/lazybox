package github

import (
	"context"
	"errors"
	"strings"

	sdk "github.com/google/go-github/v93/github"
)

// Issue is an issue GitHub created.
type Issue struct {
	Number int64
	URL    string
}

// Issues opens issues. It is separate from PullRequests so a client (or a
// test double) need not offer it; the Runner says so when it does not.
type Issues interface {
	CreateIssue(ctx context.Context, token, fullName, title, body string) (Issue, error)
}

// CreateIssue opens an issue in owner/name. A person asks for each one.
func (c *Client) CreateIssue(ctx context.Context, token, fullName, title, body string) (Issue, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return Issue{}, err
	}
	title = strings.TrimSpace(title)
	if title == "" {
		return Issue{}, errors.New("an issue needs a title")
	}
	client, err := c.client(token)
	if err != nil {
		return Issue{}, err
	}
	created, response, err := client.Issues.Create(ctx, owner, name, sdk.CreateIssueRequest{Title: title, Body: &body})
	if err != nil {
		if response != nil && response.Response != nil && response.StatusCode == 403 {
			return Issue{}, errors.New("GitHub refused to open the issue; the connection needs Issues write access (and SSO authorization for the organization)")
		}
		if response != nil && response.Response != nil && response.StatusCode == 410 {
			return Issue{}, errors.New("issues are turned off for this repository on GitHub")
		}
		return Issue{}, writeError(err, response)
	}
	return Issue{Number: int64(created.GetNumber()), URL: created.GetHTMLURL()}, nil
}
