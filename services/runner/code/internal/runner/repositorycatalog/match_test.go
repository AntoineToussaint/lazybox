package repositorycatalog

import (
	"reflect"
	"testing"
)

func names(cs []Candidate) []string {
	out := []string{}
	for _, c := range cs {
		out = append(out, c.Name)
	}
	return out
}

var catalog = []Entry{
	{Identity: "github:acme/module-directory", Name: "acme/module-directory", GitHubID: 1},
	{Identity: "github:acme/module", Name: "acme/module", GitHubID: 2},
	{Identity: "github:other/module-directory", Name: "other/module-directory", GitHubID: 3},
	{Identity: "github:acme/api", Name: "acme/api", RepositoryID: "api", LocalPath: "/src/api"},
	{Identity: "github:acme/web", Name: "acme/web", LocalPath: "/src/frontend"},
	{Identity: "local:/src/tools", Name: "tools", LocalPath: "/src/tools"},
}

func TestMatchFindsTheRepositoryAnAgentNames(t *testing.T) {
	for _, tc := range []struct {
		query string
		want  []string
	}{
		// owner/repo, in every spelling an agent might copy; another owner's
		// repository of the same name is a weaker alternative.
		{"acme/module-directory", []string{"acme/module-directory", "other/module-directory"}},
		{"github:acme/module-directory", []string{"acme/module-directory", "other/module-directory"}},
		{"https://github.com/acme/module-directory.git", []string{"acme/module-directory", "other/module-directory"}},
		{"git@github.com:acme/module-directory.git", []string{"acme/module-directory", "other/module-directory"}},
		{"https://github.com/acme/module-directory/tree/main/pkg", []string{"acme/module-directory", "other/module-directory"}},
		// A bare name finds every owner's, the closest first.
		{"module-directory", []string{"acme/module-directory", "other/module-directory"}},
		// Separators, case and a small typo do not matter.
		{"Module_Directory", []string{"acme/module-directory", "other/module-directory"}},
		{"module-directroy", []string{"acme/module-directory", "other/module-directory"}},
		// A folder named differently from its repository.
		{"frontend", []string{"acme/web"}},
		// A local path, or a folder inside one.
		{"/src/tools", []string{"tools"}},
		{"/src/api/internal/server", []string{"acme/api"}},
		// An unknown path matches by its folder name.
		{"/elsewhere/module", []string{"acme/module", "acme/module-directory"}},
		{"nothing-like-it", []string{}},
	} {
		got := Match(catalog, tc.query, 2)
		if !reflect.DeepEqual(names(got), tc.want) {
			t.Errorf("%q: got %v, want %v", tc.query, names(got), tc.want)
		}
	}
}

func TestMatchRanksExactAboveLooseAndPrefersPreparedRepositories(t *testing.T) {
	got := Match(catalog, "module", 5)
	if len(got) < 2 || got[0].Name != "acme/module" || got[0].Score != 90 || got[1].Score >= got[0].Score {
		t.Fatalf("exact name must lead: %+v", got)
	}
	exact := Match(catalog, "acme/module-directory", 5)
	if exact[0].Score != 100 || exact[0].Source != "github" {
		t.Fatalf("%+v", exact[0])
	}
	for _, c := range exact[1:] {
		if c.Name == "other/module-directory" && c.Score >= 90 {
			t.Fatalf("another owner's repository ranks too high: %+v", c)
		}
	}
	api := Match(catalog, "api", 1)
	if api[0].Source != "registered" {
		t.Fatalf("%+v", api)
	}
	// The same score: a registered repository before a GitHub one.
	twins := []Entry{{Identity: "github:z/thing", Name: "z/thing", GitHubID: 9}, {Identity: "github:y/thing", Name: "y/thing", RepositoryID: "r"}}
	if got := Match(twins, "thing", 2); got[0].Name != "y/thing" {
		t.Fatalf("%+v", got)
	}
	if Match(catalog, "  ", 3) != nil || Match(catalog, "api", 0) != nil {
		t.Fatal("empty query or limit matched")
	}
}

func TestNormalizeName(t *testing.T) {
	for in, want := range map[string]string{
		"Acme/Repo":                          "acme/repo",
		"github:acme/repo":                   "acme/repo",
		"https://github.com/acme/repo":       "acme/repo",
		"https://www.github.com/acme/repo/":  "acme/repo",
		"ssh://git@github.com/acme/repo.git": "acme/repo",
		"repo.git":                           "repo",
	} {
		if got := NormalizeName(in); got != want {
			t.Errorf("%q: %q, want %q", in, got, want)
		}
	}
}
