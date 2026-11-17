package collections

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestOriginPersistsAndRetriesNameTheSameItem(t *testing.T) {
	repo := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, repo)
	ctx := context.Background()
	spec := specTest(repo.ID)
	spec.Members[0].Branch = "812-fix-refund-rounding"
	spec.Origin = &Origin{Provider: "github", Key: "acme/api#812", Kind: "issue", Title: "Fix refund rounding", URL: "https://github.com/acme/api/issues/812", ClosingReference: "Fixes #812"}
	c, err := m.Create(ctx, "from-issue", spec)
	if err != nil {
		t.Fatal(err)
	}
	if c.Origin == nil || c.Origin.Key != "acme/api#812" || c.Origin.StartedAt.IsZero() || c.Members[0].Branch != "812-fix-refund-rounding" {
		t.Fatalf("origin not recorded: %+v", c.Origin)
	}
	// A retry that read the item again (its title changed) is the same request.
	retry := spec
	retry.Origin = &Origin{Provider: "github", Key: "acme/api#812", Title: "Fix refund rounding (again)"}
	if again, err := m.Create(ctx, "from-issue", retry); err != nil || again.ID != c.ID {
		t.Fatal("retry naming the same item is a new request", err)
	}
	// Another item under the same key is not.
	other := spec
	other.Origin = &Origin{Provider: "github", Key: "acme/api#9"}
	if _, err := m.Create(ctx, "from-issue", other); err == nil {
		t.Fatal("a different item reused the request key")
	}
	// A returned copy does not alias the record.
	c.Origin.Title = "changed"
	if got, _ := m.Get(c.ID); got.Origin.Title != "Fix refund rounding" {
		t.Fatal("origin aliased the stored record")
	}
	// Another workspace from the same item is allowed; both are found, newest first.
	time.Sleep(2 * time.Millisecond)
	second := specTest(repo.ID)
	second.Members[0].Branch = "812-fix-refund-rounding-2"
	second.Origin = &Origin{Provider: "github", Key: "ACME/api#812"}
	d, err := m.Create(ctx, "another", second)
	if err != nil {
		t.Fatal(err)
	}
	found := m.StartedFrom("github", "acme/api#812")
	if len(found) != 2 || found[0].ID != d.ID || found[1].ID != c.ID {
		t.Fatalf("started-from lookup: %+v", found)
	}
	if len(m.StartedFrom("github", "acme/api#9")) != 0 || len(m.StartedFrom("linear", "acme/api#812")) != 0 {
		t.Fatal("lookup matched another item")
	}
	if branches := m.Branches(""); !branches["812-fix-refund-rounding"] || !branches["812-fix-refund-rounding-2"] {
		t.Fatalf("branches: %v", branches)
	}
	// It survives a restart.
	m.Close()
	m = newTest(t, root, repo)
	loaded, err := m.Get(c.ID)
	if err != nil || loaded.Origin == nil || loaded.Origin.Title != "Fix refund rounding" || loaded.Origin.ClosingReference != "Fixes #812" {
		t.Fatal("origin not durable", err)
	}
}

func TestOriginIsValidated(t *testing.T) {
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	for name, origin := range map[string]*Origin{
		"no provider":   {Key: "acme/api#1"},
		"no key":        {Provider: "github"},
		"control key":   {Provider: "github", Key: "a\nb"},
		"plain address": {Provider: "github", Key: "acme/api#1", URL: "http://github.com/acme/api/issues/1"},
		"long title":    {Provider: "github", Key: "acme/api#1", Title: strings.Repeat("x", 513)},
	} {
		spec := specTest(repo.ID)
		spec.Origin = origin
		if _, err := m.Create(context.Background(), "bad-"+strings.ReplaceAll(name, " ", "-"), spec); err == nil {
			t.Errorf("%s accepted", name)
		}
	}
}

// Records written before origins existed (version 4 without the field) still
// load, with no origin; a workspace without one writes none.
func TestRecordsWithoutOriginStillLoad(t *testing.T) {
	repo := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, repo)
	c, err := m.Create(context.Background(), "older", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	m.Close()
	path := filepath.Join(root, ".lazybox-collections", "records", c.ID+".json")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var raw map[string]any
	if err = json.Unmarshal(data, &raw); err != nil {
		t.Fatal(err)
	}
	if _, has := raw["origin"]; has {
		t.Fatal("a workspace without an origin wrote one")
	}
	// As a Runner that predates origins wrote it, compactly.
	data, _ = json.Marshal(raw)
	if err = os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	m = newTest(t, root, repo)
	loaded, err := m.Get(c.ID)
	if err != nil || loaded.Origin != nil || loaded.Slug != c.Slug {
		t.Fatalf("older record: %+v %v", loaded, err)
	}
	if found := m.StartedFrom("github", "acme/api#1"); len(found) != 0 {
		t.Fatal("an older record matched an item")
	}
}
