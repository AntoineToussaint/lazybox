// Package snippets adapts the canonical Lazybox configuration catalog. It never
// reads credentials or executes snippet text; delivery remains an agent action.
package snippets

import (
	"context"
	"errors"
	"gopkg.in/yaml.v3"
	"os/exec"
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
type Command struct{ Binary, Directory string }
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
	seen := map[string]bool{}
	for _, entry := range entries {
		if entry.Key == "" || entry.Body == "" || seen[entry.Key] {
			return nil, errors.New("snippet catalog contains an empty or duplicate entry")
		}
		seen[entry.Key] = true
	}
	return entries, nil
}
