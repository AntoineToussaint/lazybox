// Package snippets adapts the canonical Lazybox configuration catalog. It never
// reads credentials or executes snippet text; delivery remains an agent action.
package snippets

import (
	"context"
	"errors"
	"fmt"
	"gopkg.in/yaml.v3"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

type Entry struct {
	Key         string   `yaml:"key"`
	Category    string   `yaml:"category"`
	Description string   `yaml:"description"`
	Body        string   `yaml:"body"`
	Origin      string   `yaml:"origin"`
	Next        []string `yaml:"next"`
}
type Catalog interface {
	List(context.Context) ([]Entry, error)
}
type Command struct {
	Binary, Directory string
	Packs             []string
}
type boundedOutput struct{ data []byte }

func (b *boundedOutput) Write(p []byte) (int, error) {
	if len(b.data)+len(p) > 4<<20 {
		return 0, errors.New("snippet catalog exceeds 4 MiB")
	}
	b.data = append(b.data, p...)
	return len(p), nil
}
func (c Command) List(ctx context.Context) ([]Entry, error) {
	if c.Binary == "" {
		return nil, errors.New("snippet catalog adapter is not configured on this Runner")
	}
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, c.Binary)
	cmd.Dir = c.Directory
	cmd.WaitDelay = time.Second
	output := &boundedOutput{}
	cmd.Stdout = output
	if err := cmd.Run(); err != nil {
		return nil, errors.New("could not load snippet catalog; check the adapter and snippet YAML on the Runner")
	}
	var entries []Entry
	if err := yaml.Unmarshal(output.data, &entries); err != nil {
		return nil, errors.New("invalid snippet catalog response")
	}
	remaining := (4 << 20) - len(output.data)
	for _, path := range c.Packs {
		pack, size, err := readPack(path, remaining)
		if err != nil {
			return nil, fmt.Errorf("snippet pack %s: %w", path, err)
		}
		remaining -= size
		entries = append(entries, pack...)
	}
	seen := map[string]bool{}
	for _, entry := range entries {
		if entry.Key == "" || entry.Body == "" || seen[entry.Key] {
			return nil, errors.New("snippet catalog contains an empty or duplicate entry")
		}
		seen[entry.Key] = true
	}
	// This workflow is implemented by the V2 Runner. Leave a same-named
	// user snippet available as ordinary text.
	if !seen["review-pr-ci"] {
		entries = append(entries, Entry{Key: "review-pr-ci", Category: "Review", Origin: "workflow", Description: "Review with the default reviewer, fix findings, open or update the PR, and repair failing CI.", Body: "Start an independent code review with my default reviewer. Return the completed report to this conversation. Apply fixes here, create or update the pull request, and monitor and repair its CI until checks pass or a blocker needs my decision. Leave the PR open."})
	}
	return entries, nil
}

func readPack(path string, budget int) ([]Entry, int, error) {
	if !filepath.IsAbs(path) {
		return nil, 0, errors.New("expected an absolute path")
	}
	file, err := os.Open(path)
	if err != nil {
		return nil, 0, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return nil, 0, err
	}
	if !info.Mode().IsRegular() {
		return nil, 0, errors.New("expected a regular file")
	}
	data, err := io.ReadAll(io.LimitReader(file, int64(budget+1)))
	if err != nil {
		return nil, 0, err
	}
	if len(data) > budget {
		return nil, 0, errors.New("combined catalog exceeds 4 MiB")
	}
	var entries []Entry
	if err := yaml.Unmarshal(data, &entries); err != nil {
		return nil, 0, err
	}
	for i := range entries {
		entries[i].Origin = "pack: " + filepath.Base(path)
	}
	return entries, len(data), nil
}
