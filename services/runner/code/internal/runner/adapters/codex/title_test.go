package codex

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// fakeTitleServer is an app-server that answers one naming turn: it lists a
// "mini" model, starts the naming thread only as an ephemeral read-only one
// on that model, and answers with JSON, after a line from another thread.
const fakeTitleServer = `while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\(lazybox-[0-9]*\)".*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":"%s","result":{}}\n' "$id" ;;
    *'"method":"model/list"'*) printf '{"id":"%s","result":{"data":[{"id":"gpt-5.1-codex","model":"gpt-5.1-codex"},{"id":"gpt-5.1-codex-mini","model":"gpt-5.1-codex-mini"}]}}\n' "$id" ;;
    *'"method":"thread/start"'*'"ephemeral":true,"model":"gpt-5.1-codex-mini","sandbox":"read-only"'*) printf '{"id":"%s","result":{"thread":{"id":"t-1"}}}\n' "$id" ;;
    *'"method":"turn/start"'*'"outputSchema"'*'"threadId":"t-1"'*)
      printf '{"id":"%s","result":{"turn":{"id":"u-1"}}}\n' "$id"
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"other","item":{"type":"agentMessage","text":"wrong"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"t-1","item":{"type":"agentMessage","text":"{\"title\":\"Login flake on WebKit\"}"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"t-1","turn":{"id":"u-1","status":"completed"}}}' ;;
    *'"id":"lazybox-'*) printf '{"id":"%s","error":{"message":"unexpected"}}\n' "$id" ;;
  esac
done
`

func TestCodexNamesAConversationInAnEphemeralTurnOfItsOwn(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	server := filepath.Join(t.TempDir(), "codex")
	if err := os.WriteFile(server, []byte("#!/bin/sh\n"+fakeTitleServer), 0700); err != nil {
		t.Fatal(err)
	}
	s := &session{titler: newTitler(server, agent.Open{Cwd: t.TempDir(), Env: os.Environ()}, func() string { return "gpt-5.1-codex" })}
	title, err := agent.TitleOf(ctx, s, "the login test is flaky on WebKit")
	if err != nil || title != "Login flake on WebKit" {
		t.Fatalf("title %q, %v", title, err)
	}
}

func TestATitleAnswerOutsideTheSchemaIsStillRead(t *testing.T) {
	if got := parseTitle(`{"title":"Runner restarts"}`); got != "Runner restarts" {
		t.Fatal(got)
	}
	if got := parseTitle("  Runner restarts \n"); got != "Runner restarts" {
		t.Fatal(got)
	}
}
