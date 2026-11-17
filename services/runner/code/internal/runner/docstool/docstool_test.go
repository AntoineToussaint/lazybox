package docstool

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func docs(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	for path, text := range map[string]string{
		"README.md":               "# Lazybox V2 documentation\n\nStart with getting started.\n",
		"guides/subscriptions.md": "# Subscriptions\n\nAdd a subscription in Settings → Subscriptions.\n\n## When a subscription reaches its limit\n\nLazybox switches to another subscription.\n",
		"guides/chat.md":          "# Chat\n\nSend a message with Enter. A subscription limit pauses the conversation.\n",
		"internal/README.md":      "# Internal\n\nSubscriptions internals for maintainers.\n",
		"images/hero.webp":        "not markdown",
	} {
		full := filepath.Join(dir, filepath.FromSlash(path))
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, []byte(text), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	return dir
}

func TestPagesListEveryPublicPageWithItsTitle(t *testing.T) {
	pages, err := Pages(docs(t))
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, p := range pages {
		got = append(got, p.Path+"="+p.Title)
	}
	if strings.Join(got, ",") != "README.md=Lazybox V2 documentation,guides/chat.md=Chat,guides/subscriptions.md=Subscriptions" {
		t.Fatal(got)
	}
	if index := Index(docs(t)); !strings.Contains(index, "- guides/chat.md — Chat\n") || strings.Contains(index, "internal") {
		t.Fatal(index)
	}
	if _, err = Pages(""); !errors.Is(err, ErrUnavailable) {
		t.Fatal(err)
	}
	if _, err = Pages(filepath.Join(t.TempDir(), "missing")); !errors.Is(err, ErrUnavailable) {
		t.Fatal(err)
	}
	if Index("") != "" {
		t.Fatal("an index without documentation")
	}
}

func TestSearchRanksThePageAboutTheTopicFirst(t *testing.T) {
	dir := docs(t)
	hits, err := Search(dir, "How do I switch when a subscription hits its limit?")
	if err != nil {
		t.Fatal(err)
	}
	if len(hits) != 2 || hits[0].Path != "guides/subscriptions.md" || hits[1].Path != "guides/chat.md" {
		t.Fatalf("%+v", hits)
	}
	if len(hits[0].Excerpt) == 0 || !strings.Contains(strings.Join(hits[0].Excerpt, "\n"), "limit") {
		t.Fatalf("excerpt %+v", hits[0].Excerpt)
	}
	if hits, _ = Search(dir, "maintainers"); len(hits) != 0 {
		t.Fatalf("internal pages are searched: %+v", hits)
	}
	if _, err = Search(dir, "  ? "); err == nil {
		t.Fatal("an empty query was accepted")
	}
}

func TestReadServesOnlyPagesInsideTheDocsFolder(t *testing.T) {
	dir := docs(t)
	text, err := Read(dir, "guides/chat.md")
	if err != nil || !strings.HasPrefix(text, "# Chat") {
		t.Fatal(text, err)
	}
	if text, err = Read(dir, "docs/guides/chat.md"); err != nil || !strings.HasPrefix(text, "# Chat") {
		t.Fatal("a docs/ prefix is the same page", err)
	}
	outside := filepath.Join(t.TempDir(), "secret.md")
	if err = os.WriteFile(outside, []byte("secret"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(outside, filepath.Join(dir, "guides", "link.md")); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{"../secret.md", "/etc/hosts", outside, "internal/README.md", "images/hero.webp", "guides/link.md", "guides/missing.md", ""} {
		if _, err := Read(dir, path); err == nil {
			t.Errorf("read %q", path)
		}
	}
}

func TestHandleRunsTheDocumentationTools(t *testing.T) {
	dir := docs(t)
	out, err := Handle(dir, agent.DocsSearchTool, json.RawMessage(`{"query":"subscription limit"}`))
	if err != nil || !strings.Contains(out, `"path":"guides/subscriptions.md"`) {
		t.Fatal(out, err)
	}
	if out, err = Handle(dir, agent.DocsReadTool, json.RawMessage(`{"path":"guides/subscriptions.md"}`)); err != nil || !strings.Contains(out, "Settings → Subscriptions") {
		t.Fatal(out, err)
	}
	if _, err = Handle(dir, agent.SettingsReadTool, nil); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatal(err)
	}
	if _, err = Handle("", agent.DocsSearchTool, json.RawMessage(`{"query":"chat"}`)); !errors.Is(err, ErrUnavailable) {
		t.Fatal(err)
	}
}
