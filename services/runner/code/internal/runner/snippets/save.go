package snippets

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"regexp"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/atomicfile"
	"gopkg.in/yaml.v3"
)

var snippetKey = regexp.MustCompile("^[a-z0-9][a-z0-9_-]{0,63}$")

// Save adds one plain-text proposal. The owning service serializes saves.
func (c Command) Save(ctx context.Context, proposal string) (Entry, error) {
	var zero Entry
	if len(proposal) > 64<<10 {
		return zero, errors.New("snippet proposal exceeds 64 KiB")
	}
	var rows []struct{ Key, Category, Description, Body string }
	decoder := yaml.NewDecoder(strings.NewReader(proposal))
	decoder.KnownFields(true)
	if err := decoder.Decode(&rows); err != nil || len(rows) != 1 {
		return zero, errors.New("expected one YAML list entry with key, category, description and body")
	}
	if err := decoder.Decode(new(any)); err != io.EOF {
		return zero, errors.New("expected one YAML document")
	}
	r := rows[0]
	if !snippetKey.MatchString(r.Key) || strings.TrimSpace(r.Body) == "" || len(r.Body) > 32768 || len(r.Description) > 512 || len(r.Category) > 128 {
		return zero, errors.New("use a short lowercase snippet key and a non-empty body of at most 32 KiB")
	}
	entry := Entry{Key: r.Key, Body: r.Body, Category: r.Category, Description: r.Description, Origin: "saved"}
	var budget int
	catalog, err := c.list(ctx, &budget)
	if err != nil {
		return zero, err
	}
	for _, existing := range catalog {
		if existing.Key != entry.Key {
			continue
		}
		if existing.Origin == "saved" && existing.Category == entry.Category && existing.Description == entry.Description && existing.Body == entry.Body {
			return existing, nil
		}
		return zero, fmt.Errorf("snippet %q already exists; choose another key", entry.Key)
	}
	saved, _, err := readPack(c.SavedPack, 4<<20)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return zero, err
	}
	data, err := yaml.Marshal(append(saved, entry))
	if err != nil {
		return zero, err
	}
	if len(data) > budget {
		return zero, errors.New("combined snippet catalog exceeds 4 MiB")
	}
	return entry, atomicfile.WriteFile(c.SavedPack, data)
}
