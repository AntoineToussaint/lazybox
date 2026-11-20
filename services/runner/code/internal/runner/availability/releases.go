package availability

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"regexp"
	"strings"
	"sync"
	"time"
)

var releaseURLs = map[string]string{
	"claude": "https://api.github.com/repos/anthropics/claude-code/releases/latest",
	"codex":  "https://api.github.com/repos/openai/codex/releases/latest",
}

var stableVersion = regexp.MustCompile(`^[0-9]+\.[0-9]+\.[0-9]+$`)

type releaseCheck struct {
	next    time.Time
	version string
}

// ReleaseCache shares public release lookups across the Runner's windows.
type ReleaseCache struct {
	mu      sync.Mutex
	entries map[string]releaseCheck
	client  *http.Client
}

func (c *ReleaseCache) Latest(ctx context.Context, provider string) string {
	url := releaseURLs[provider]
	if url == "" || ctx.Err() != nil {
		return ""
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	old := c.entries[provider]
	if time.Now().Before(old.next) {
		return old.version
	}
	client := c.client
	if client == nil {
		client = http.DefaultClient
	}
	request, cancel := context.WithTimeout(ctx, 4*time.Second)
	defer cancel()
	version, err := checkRelease(request, client, url)
	if ctx.Err() != nil {
		return ""
	}
	if c.entries == nil {
		c.entries = make(map[string]releaseCheck)
	}
	if err != nil {
		c.entries[provider] = releaseCheck{next: time.Now().Add(15 * time.Minute)}
		return ""
	}
	c.entries[provider] = releaseCheck{next: time.Now().Add(6 * time.Hour), version: version}
	return version
}

func checkRelease(ctx context.Context, client *http.Client, url string) (string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	resp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("release feed returned HTTP %d", resp.StatusCode)
	}
	var release struct {
		Tag        string `json:"tag_name"`
		Draft      bool   `json:"draft"`
		Prerelease bool   `json:"prerelease"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&release); err != nil {
		return "", err
	}
	version := strings.TrimPrefix(strings.TrimPrefix(release.Tag, "rust-"), "v")
	if release.Draft || release.Prerelease || !stableVersion.MatchString(version) {
		return "", fmt.Errorf("release feed did not report a stable version")
	}
	return version, nil
}
