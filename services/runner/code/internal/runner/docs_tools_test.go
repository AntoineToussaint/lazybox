package runner

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

// The help assistant answers from the real documentation: its briefing lists
// the pages, and its documentation tools search and read them. No other
// agent gets them.
func TestTheHelpAssistantReadsTheDocumentation(t *testing.T) {
	docs := t.TempDir()
	if err := os.MkdirAll(filepath.Join(docs, "guides"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(docs, "guides", "subscriptions.md"), []byte("# Subscriptions\n\nAdd one in Settings → Subscriptions.\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	b, err := New(Config{State: t.TempDir(), DocsDirectory: docs})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["claude"] = fake
	ctx := context.Background()
	if _, err = b.OpenAssistant("claude-default", "help", "", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	help := fake.Session.Open
	if !strings.Contains(help.Context, "lazybox_search_docs") || !strings.Contains(help.Context, "- guides/subscriptions.md — Subscriptions") {
		t.Fatalf("briefing without the documentation:\n%s", help.Context)
	}
	found, err := help.Tools(ctx, agent.DocsSearchTool, json.RawMessage(`{"query":"add a subscription"}`))
	if err != nil || !strings.Contains(found, "guides/subscriptions.md") {
		t.Fatal(found, err)
	}
	page, err := help.Tools(ctx, agent.DocsReadTool, json.RawMessage(`{"path":"guides/subscriptions.md"}`))
	if err != nil || !strings.Contains(page, "Settings → Subscriptions") {
		t.Fatal(page, err)
	}

	if _, err = b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	coding := fake.Session.Open
	if strings.Contains(coding.Context, "lazybox_search_docs") {
		t.Fatal("a coding agent was briefed on the documentation tools")
	}
	if _, err = coding.Tools(ctx, agent.DocsReadTool, json.RawMessage(`{"path":"guides/subscriptions.md"}`)); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("a coding agent read the documentation through the Runner: %v", err)
	}
}

// Without documentation the help assistant is told so, and still opens.
func TestTheHelpAssistantOpensWithoutDocumentation(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["claude"] = fake
	if _, err = b.OpenAssistant("claude-default", "help", "", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(fake.Session.Open.Context, "not available on this Runner") {
		t.Fatal(fake.Session.Open.Context)
	}
}
