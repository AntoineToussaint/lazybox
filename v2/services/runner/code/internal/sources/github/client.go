// Package github implements GitHub's wire protocol without Runner or UI state.
package github

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	sdk "github.com/google/go-github/v93/github"
)

var ErrUnauthorized = errors.New("GitHub sign-in expired or revoked; reconnect this Lazybox account")
var ErrForbidden = errors.New("GitHub denied access; check token permissions, organization or SSO policy, App access if applicable, and rate limits")

type Token struct {
	AccessToken      string `json:"access_token"`
	RefreshToken     string `json:"refresh_token"`
	ExpiresIn        int64  `json:"expires_in"`
	RefreshExpiresIn int64  `json:"refresh_token_expires_in"`
	Error            string `json:"error"`
}
type Device struct {
	DeviceCode      string `json:"device_code"`
	UserCode        string `json:"user_code"`
	VerificationURI string `json:"verification_uri"`
	ExpiresIn       int64  `json:"expires_in"`
	Interval        int64  `json:"interval"`
}
type User struct {
	ID    int64  `json:"id"`
	Login string `json:"login"`
}
type Installation struct {
	ID                  int64  `json:"id"`
	Account             User   `json:"account"`
	RepositorySelection string `json:"repository_selection"`
}
type Repository struct {
	ID            int64  `json:"id"`
	Name          string `json:"name"`
	FullName      string `json:"full_name"`
	Private       bool   `json:"private"`
	DefaultBranch string `json:"default_branch"`
	HTMLURL       string `json:"html_url"`
	CloneURL      string `json:"clone_url"`
	Archived      bool   `json:"archived"`
	Disabled      bool   `json:"disabled"`
}
type Item struct {
	ID          int64  `json:"id"`
	Number      int64  `json:"number"`
	Title       string `json:"title"`
	State       string `json:"state"`
	HTMLURL     string `json:"html_url"`
	UpdatedAt   string `json:"updated_at"`
	User        User   `json:"user"`
	PullRequest *struct {
		URL string `json:"url"`
	} `json:"pull_request,omitempty"`
}

// API is the provider boundary used by the Runner connection manager.
type API interface {
	Begin(context.Context, string) (Device, error)
	Exchange(context.Context, string, string) (Token, error)
	Refresh(context.Context, string, string) (Token, error)
	User(context.Context, string) (User, error)
	Installations(context.Context, string, int) ([]Installation, bool, error)
	Repositories(context.Context, string, int64, int) ([]Repository, bool, error)
	UserRepositories(context.Context, string, int) ([]Repository, bool, error)
	Repository(context.Context, string, int64) (Repository, error)
	Items(context.Context, string, string, int) ([]Item, bool, error)
}
type Client struct {
	http             *http.Client
	apiURL, loginURL string
}

func New() *Client {
	return &Client{http: &http.Client{Timeout: 20 * time.Second, CheckRedirect: func(*http.Request, []*http.Request) error { return errors.New("GitHub redirects are not followed") }}, apiURL: "https://api.github.com", loginURL: "https://github.com"}
}
func (c *Client) request(ctx context.Context, method, target, token string, form url.Values, out any) error {
	var body io.Reader
	if form != nil {
		body = strings.NewReader(form.Encode())
	}
	req, err := http.NewRequestWithContext(ctx, method, target, body)
	if err != nil {
		return errors.New("invalid GitHub request")
	}
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Lazybox-V2")
	if form != nil {
		req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
		req.Header.Set("X-GitHub-Api-Version", "2026-03-10")
	}
	res, err := c.http.Do(req)
	if err != nil {
		return errors.New("GitHub request failed; check the network connection")
	}
	defer res.Body.Close()
	if res.StatusCode == 401 {
		return ErrUnauthorized
	}
	if res.StatusCode == 403 || res.StatusCode == 429 {
		return ErrForbidden
	}
	if res.StatusCode < 200 || res.StatusCode >= 300 {
		return fmt.Errorf("GitHub returned HTTP %d", res.StatusCode)
	}
	data, err := io.ReadAll(io.LimitReader(res.Body, (4<<20)+1))
	if err != nil || len(data) > 4<<20 {
		return errors.New("GitHub response could not be read within its size limit")
	}
	if json.NewDecoder(bytes.NewReader(data)).Decode(out) != nil {
		return errors.New("GitHub returned an invalid response")
	}
	return nil
}
func (c *Client) Begin(ctx context.Context, client string) (Device, error) {
	var d Device
	err := c.request(ctx, "POST", c.loginURL+"/login/device/code", "", url.Values{"client_id": {client}, "scope": {"offline_access"}}, &d)
	if err == nil && (d.DeviceCode == "" || d.UserCode == "" || d.ExpiresIn <= 0 || d.ExpiresIn > 3600 || d.Interval < 0 || d.Interval > 60 || d.VerificationURI != "https://github.com/login/device") {
		err = errors.New("GitHub device sign-in unavailable; verify the App client ID and enable Device Flow")
	}
	return d, err
}
func (c *Client) token(ctx context.Context, values url.Values) (Token, error) {
	var t Token
	err := c.request(ctx, "POST", c.loginURL+"/login/oauth/access_token", "", values, &t)
	return t, err
}
func (c *Client) Exchange(ctx context.Context, client, device string) (Token, error) {
	return c.token(ctx, url.Values{"client_id": {client}, "device_code": {device}, "grant_type": {"urn:ietf:params:oauth:grant-type:device_code"}})
}
func (c *Client) Refresh(ctx context.Context, client, refresh string) (Token, error) {
	return c.token(ctx, url.Values{"client_id": {client}, "refresh_token": {refresh}, "grant_type": {"refresh_token"}})
}

// Every operation gets its own authenticated SDK client. Tokens never become
// global state or leak into another account's requests. SDK errors are redacted
// at this boundary: upstream bodies, URLs and headers are not user-facing errors.
func (c *Client) apiClient(token string) (*sdk.Client, error) {
	base := strings.TrimRight(c.apiURL, "/") + "/"
	httpClient := *c.http
	transport := httpClient.Transport
	if transport == nil {
		transport = http.DefaultTransport
	}
	httpClient.Transport = boundedTransport{transport}
	return sdk.NewClient(sdk.WithHTTPClient(&httpClient), sdk.WithURLs(&base, &base), sdk.WithAuthToken(token), sdk.WithUserAgent("Lazybox-V2"))
}

type boundedTransport struct{ http.RoundTripper }

func (t boundedTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	response, err := t.RoundTripper.RoundTrip(req)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	data, err := io.ReadAll(io.LimitReader(response.Body, (4<<20)+1))
	if err != nil || len(data) > 4<<20 {
		return nil, errors.New("GitHub response exceeded its size limit or could not be read")
	}
	response.Body = io.NopCloser(bytes.NewReader(data))
	return response, nil
}
func apiError(err error, response *sdk.Response) error {
	if err == nil {
		return nil
	}
	if errors.Is(err, context.Canceled) {
		return context.Canceled
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return context.DeadlineExceeded
	}
	if response != nil && response.Response != nil {
		switch response.StatusCode {
		case 401:
			return ErrUnauthorized
		case 403, 429:
			return ErrForbidden
		default:
			return fmt.Errorf("GitHub request failed (HTTP %d)", response.StatusCode)
		}
	}
	return errors.New("GitHub request failed; check the connection and try again")
}
func listOptions(page int) (*sdk.ListOptions, error) {
	if page < 1 || page > 1000 {
		return nil, errors.New("GitHub page must be between 1 and 1000")
	}
	return &sdk.ListOptions{Page: page, PerPage: 100}, nil
}
func hasMore(response *sdk.Response) bool { return response != nil && response.NextPage > 0 }
func (c *Client) User(ctx context.Context, token string) (User, error) {
	client, err := c.apiClient(token)
	if err != nil {
		return User{}, errors.New("could not initialize GitHub client")
	}
	user, response, err := client.Users.Get(ctx, "")
	if err != nil {
		return User{}, apiError(err, response)
	}
	if user.GetID() <= 0 || user.GetLogin() == "" {
		return User{}, errors.New("GitHub omitted the authenticated identity")
	}
	return User{ID: user.GetID(), Login: user.GetLogin()}, nil
}
func (c *Client) Installations(ctx context.Context, token string, page int) ([]Installation, bool, error) {
	options, err := listOptions(page)
	if err != nil {
		return nil, false, err
	}
	client, err := c.apiClient(token)
	if err != nil {
		return nil, false, errors.New("could not initialize GitHub client")
	}
	items, response, err := client.Apps.ListUserInstallations(ctx, options)
	if err != nil {
		return nil, false, apiError(err, response)
	}
	out := make([]Installation, 0, len(items))
	for _, item := range items {
		out = append(out, Installation{ID: item.GetID(), Account: User{ID: item.GetAccount().GetID(), Login: item.GetAccount().GetLogin()}, RepositorySelection: item.GetRepositorySelection()})
	}
	return out, hasMore(response), nil
}
func mapRepository(repo *sdk.Repository) Repository {
	return Repository{ID: repo.GetID(), Name: repo.GetName(), FullName: repo.GetFullName(), Private: repo.GetPrivate(), DefaultBranch: repo.GetDefaultBranch(), HTMLURL: repo.GetHTMLURL(), CloneURL: repo.GetCloneURL(), Archived: repo.GetArchived(), Disabled: repo.GetDisabled()}
}
func mapRepositories(repos []*sdk.Repository) []Repository {
	out := make([]Repository, 0, len(repos))
	for _, repo := range repos {
		out = append(out, mapRepository(repo))
	}
	return out
}
func (c *Client) Repositories(ctx context.Context, token string, installation int64, page int) ([]Repository, bool, error) {
	options, err := listOptions(page)
	if err != nil {
		return nil, false, err
	}
	if installation <= 0 {
		return nil, false, errors.New("installation required")
	}
	client, err := c.apiClient(token)
	if err != nil {
		return nil, false, errors.New("could not initialize GitHub client")
	}
	items, response, err := client.Apps.ListUserRepos(ctx, installation, options)
	if err != nil {
		return nil, false, apiError(err, response)
	}
	return mapRepositories(items.Repositories), hasMore(response), nil
}

// UserRepositories lists the authenticated user's accessible repositories,
// including collaborator and organization membership, without App installations.
func (c *Client) UserRepositories(ctx context.Context, token string, page int) ([]Repository, bool, error) {
	options, err := listOptions(page)
	if err != nil {
		return nil, false, err
	}
	client, err := c.apiClient(token)
	if err != nil {
		return nil, false, errors.New("could not initialize GitHub client")
	}
	items, response, err := client.Repositories.ListByAuthenticatedUser(ctx, &sdk.RepositoryListByAuthenticatedUserOptions{Affiliation: "owner,collaborator,organization_member", Sort: "full_name", Direction: "asc", ListOptions: *options})
	if err != nil {
		return nil, false, apiError(err, response)
	}
	return mapRepositories(items), hasMore(response), nil
}
func (c *Client) Repository(ctx context.Context, token string, id int64) (Repository, error) {
	if id <= 0 {
		return Repository{}, errors.New("repository required")
	}
	client, err := c.apiClient(token)
	if err != nil {
		return Repository{}, errors.New("could not initialize GitHub client")
	}
	repo, response, err := client.Repositories.GetByID(ctx, id)
	if err != nil {
		return Repository{}, apiError(err, response)
	}
	return mapRepository(repo), nil
}
func (c *Client) Items(ctx context.Context, token, fullName string, page int) ([]Item, bool, error) {
	options, err := listOptions(page)
	if err != nil {
		return nil, false, err
	}
	parts := strings.Split(fullName, "/")
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
		return nil, false, errors.New("repository owner/name required")
	}
	client, err := c.apiClient(token)
	if err != nil {
		return nil, false, errors.New("could not initialize GitHub client")
	}
	items, response, err := client.Issues.ListByRepo(ctx, parts[0], parts[1], &sdk.IssueListByRepoOptions{State: "open", Sort: "updated", Direction: "desc", ListOptions: *options})
	if err != nil {
		return nil, false, apiError(err, response)
	}
	out := make([]Item, 0, len(items))
	for _, issue := range items {
		item := Item{ID: issue.GetID(), Number: int64(issue.GetNumber()), Title: issue.GetTitle(), State: issue.GetState(), HTMLURL: issue.GetHTMLURL(), User: User{ID: issue.GetUser().GetID(), Login: issue.GetUser().GetLogin()}}
		if issue.UpdatedAt != nil {
			item.UpdatedAt = issue.UpdatedAt.Format(time.RFC3339)
		}
		if issue.PullRequestLinks != nil {
			item.PullRequest = &struct {
				URL string `json:"url"`
			}{URL: issue.PullRequestLinks.GetURL()}
		}
		out = append(out, item)
	}
	return out, hasMore(response), nil
}
