package github

import (
	"context"
	"fmt"
	"net/url"
	"strconv"
	"strings"
)

type Notification struct {
	ID, Repository, Title, Reason, URL, UpdatedAt string
	RepositoryID                                  int64
}

// Mailbox reads the authenticated user's unread issue and pull request threads.
func (c *Client) Mailbox(ctx context.Context, token string) ([]Notification, bool, error) {
	const pageSize = 100
	const maxPages = 3
	var items []Notification
	seen := map[string]bool{}
	for page := 1; page <= maxPages; page++ {
		var threads []struct {
			ID         string
			Reason     string
			UpdatedAt  string `json:"updated_at"`
			Repository Repository
			Subject    struct{ Title, URL, Type string }
		}
		target := fmt.Sprintf("%s/notifications?per_page=%d&page=%d", c.apiURL, pageSize, page)
		if err := c.request(ctx, "GET", target, token, nil, &threads); err != nil {
			return nil, false, err
		}
		for _, thread := range threads {
			if thread.ID == "" || seen[thread.ID] {
				continue
			}
			link := notificationLink(thread.Repository.FullName, thread.Subject.URL, thread.Subject.Type)
			if link == "" {
				continue
			}
			seen[thread.ID] = true
			items = append(items, Notification{thread.ID, thread.Repository.FullName, thread.Subject.Title, thread.Reason, link, thread.UpdatedAt, thread.Repository.ID})
		}
		if len(threads) < pageSize {
			return items, false, nil
		}
	}
	return items, true, nil
}

// Build the public link from validated repository and item identifiers.
func notificationLink(repo, target, kind string) string {
	path, err := url.Parse(target)
	name := strings.Split(repo, "/")
	if err != nil || len(name) != 2 {
		return ""
	}
	for _, part := range name {
		if part == "" || part == "." || part == ".." || url.PathEscape(part) != part || strings.ContainsAny(part, ":%@") {
			return ""
		}
	}
	parts := strings.Split(strings.Trim(path.Path, "/"), "/")
	if len(parts) != 5 || parts[0] != "repos" || !strings.EqualFold(strings.Join(parts[1:3], "/"), repo) {
		return ""
	}
	number, err := strconv.ParseUint(parts[4], 10, 64)
	if err != nil || number == 0 {
		return ""
	}
	switch {
	case kind == "Issue" && parts[3] == "issues":
		return fmt.Sprintf("https://github.com/%s/issues/%d", repo, number)
	case kind == "PullRequest" && (parts[3] == "pulls" || parts[3] == "issues"):
		return fmt.Sprintf("https://github.com/%s/pull/%d", repo, number)
	default:
		return ""
	}
}
