package claude

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestGatewayEnvironmentPreservesAuthAndNeverMutatesParent(t *testing.T) {
	parent := []string{"ANTHROPIC_BASE_URL=https://old.test", "ANTHROPIC_API_KEY=provider-secret", "BENCH_TOKEN=gateway-secret"}
	result := GatewayEnv(&agent.Gateway{ID: "meter", URL: "https://meter.test/route/"}, parent)
	joined := strings.Join(result, "\n")
	if strings.Count(joined, "ANTHROPIC_BASE_URL=") != 1 || !strings.Contains(joined, "ANTHROPIC_API_KEY=provider-secret") || !strings.Contains(joined, "ANTHROPIC_BASE_URL=https://meter.test/route") {
		t.Fatal("incorrect gateway or provider credential mapping")
	}
	result = GatewayEnv(&agent.Gateway{ID: "meter", URL: "https://meter.test", TokenEnv: "BENCH_TOKEN"}, parent)
	joined = strings.Join(result, "\n")
	if strings.Contains(joined, "ANTHROPIC_API_KEY=") || !strings.Contains(joined, "ANTHROPIC_AUTH_TOKEN=gateway-secret") {
		t.Fatal("gateway token mode retained competing auth")
	}
	if parent[0] != "ANTHROPIC_BASE_URL=https://old.test" || parent[1] != "ANTHROPIC_API_KEY=provider-secret" {
		t.Fatal("parent environment mutated")
	}
}

// stopWorker answers open with the given features, then acknowledges a
// stop_agent call only for the native task it expects.
func stopWorker(features string) string {
	return `read -r line
printf '%s\n' '{"id":"lazybox-1","result":{"reference":"ref","features":` + features + `}}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"stop_agent"'*'"nativeId":"task-7"'*) printf '%s\n' '{"id":"lazybox-2","result":{"delivery":"acknowledged"}}' ;;
    *) printf '%s\n' '{"id":"lazybox-2","error":{"message":"unexpected"}}' ;;
  esac
done
`
}

func TestStopAgentReachesOnlyAWorkerThatSupportsIt(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	for _, tc := range []struct {
		features string
		works    bool
	}{{`["permission","context","stop_agent"]`, true}, {`["permission","context"]`, false}} {
		worker := filepath.Join(t.TempDir(), "worker.sh")
		if err := os.WriteFile(worker, []byte(stopWorker(tc.features)), 0700); err != nil {
			t.Fatal(err)
		}
		session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, agent.Open{Cwd: t.TempDir()})
		if err != nil {
			t.Fatal(err)
		}
		if session.Capabilities().StopAgent != tc.works {
			t.Fatalf("features %s: StopAgent capability %v", tc.features, !tc.works)
		}
		r, err := session.Apply(ctx, agent.Operation{ID: "stop", Kind: agent.StopAgent, AgentID: "claude-tool:x", NativeID: "task-7"})
		if tc.works && (err != nil || r.Delivery != "acknowledged" || r.ID != "stop") {
			t.Fatalf("stop not acknowledged: %+v %v", r, err)
		}
		if !tc.works && !errors.Is(err, agent.ErrUnsupported) {
			t.Fatalf("an older worker was asked to stop a sub-agent: %v", err)
		}
		_ = session.Close()
	}
}

// A conversation is named by the worker that offers it (Claude Code's own
// title generator); an older worker is not asked.
func TestTitleReachesOnlyAWorkerThatSupportsIt(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	script := func(features string) string {
		return `read -r line
printf '%s\n' '{"id":"lazybox-1","result":{"reference":"ref","features":` + features + `}}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"title"'*'"description":"fix the login flake"'*) printf '%s\n' '{"id":"lazybox-2","result":{"title":"Login flake"}}' ;;
    *) printf '%s\n' '{"id":"lazybox-2","error":{"message":"unexpected"}}' ;;
  esac
done
`
	}
	for _, tc := range []struct {
		features string
		works    bool
	}{{`["permission","title"]`, true}, {`["permission"]`, false}} {
		worker := filepath.Join(t.TempDir(), "worker.sh")
		if err := os.WriteFile(worker, []byte(script(tc.features)), 0700); err != nil {
			t.Fatal(err)
		}
		session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, agent.Open{Cwd: t.TempDir()})
		if err != nil {
			t.Fatal(err)
		}
		title, err := agent.TitleOf(ctx, session, "fix the login flake")
		if tc.works && (err != nil || title != "Login flake") {
			t.Fatalf("title not generated: %q %v", title, err)
		}
		if !tc.works && !errors.Is(err, agent.ErrUnsupported) {
			t.Fatalf("an older worker was asked for a title: %v", err)
		}
		_ = session.Close()
	}
}

// The person's claude.ai connectors setting reaches the worker at open: off
// unless they turned it on (Settings → Agents).
func TestOpenTellsTheWorkerWhetherClaudeAIConnectorsLoad(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	for _, tc := range []struct {
		on   bool
		want string
	}{{false, `"claudeAiConnectors":false`}, {true, `"claudeAiConnectors":true`}} {
		worker := filepath.Join(t.TempDir(), "worker.sh")
		script := `read -r line
case "$line" in
  *'"method":"open"'*'` + tc.want + `'*) printf '%s\n' '{"id":"lazybox-1","result":{"reference":"ref","features":[]}}' ;;
  *) printf '%s\n' '{"id":"lazybox-1","error":{"message":"connectors setting missing"}}' ;;
esac
while IFS= read -r line; do :; done
`
		if err := os.WriteFile(worker, []byte(script), 0700); err != nil {
			t.Fatal(err)
		}
		session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, agent.Open{Cwd: t.TempDir(), ClaudeAIConnectors: tc.on})
		if err != nil {
			t.Fatalf("connectors %v: the worker was not told %s: %v", tc.on, tc.want, err)
		}
		_ = session.Close()
	}
}

// A worker line the Runner cannot read is skipped, never the end of
// supervision: the worker keeps running, so Receive goes on to its next event.
// The first skip is recorded; an unreadable tool call is refused.
func TestUndecodableWorkerLineIsSkipped(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	worker := filepath.Join(t.TempDir(), "worker.sh")
	script := `read -r line
printf '%s\n' '{"id":"lazybox-1","result":{"reference":"ref","features":[]}}'
printf '%s\n' '{"method":"event","params":{"type":5}}'
printf '%s\n' '{"method":"event","params":{"text":["not","text"]}}'
printf '%s\n' '{"id":"tool-1","method":"tool","params":{"name":7}}'
while IFS= read -r line; do
  case "$line" in
    *'"id":"tool-1"'*'"error"'*) printf '%s\n' '{"method":"event","params":{"type":"message_delta","text":"tool refused"}}' ;;
  esac
done
`
	if err := os.WriteFile(worker, []byte(script), 0700); err != nil {
		t.Fatal(err)
	}
	session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, agent.Open{Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	ev, err := session.Receive(ctx)
	if err != nil || ev.Type != agent.UnreadableEvent || !strings.Contains(ev.Text, "could not read") {
		t.Fatalf("first unreadable line: %+v %v", ev, err)
	}
	// The second unreadable event and the unreadable tool call are skipped
	// without a record; the worker hears the refusal and goes on.
	ev, err = session.Receive(ctx)
	if err != nil || ev.Type != "message_delta" || ev.Text != "tool refused" {
		t.Fatalf("after unreadable lines: %+v %v", ev, err)
	}
}

// Steering reaches a worker that offers it, with the message as its content
// parts; an older worker is not asked, and the message waits for the turn.
func TestSteerReachesOnlyAWorkerThatSupportsIt(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	script := func(features string) string {
		return `read -r line
printf '%s\n' '{"id":"lazybox-1","result":{"reference":"ref","features":` + features + `}}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"steer"'*'"parts":[{"type":"text","text":"Also log the port."}]'*) printf '%s\n' '{"id":"lazybox-2","result":{"delivery":"submitted"}}' ;;
    *) printf '%s\n' '{"id":"lazybox-2","error":{"message":"unexpected"}}' ;;
  esac
done
`
	}
	for _, tc := range []struct {
		features string
		works    bool
	}{{`["permission","parts","steer"]`, true}, {`["permission","parts"]`, false}} {
		worker := filepath.Join(t.TempDir(), "worker.sh")
		if err := os.WriteFile(worker, []byte(script(tc.features)), 0700); err != nil {
			t.Fatal(err)
		}
		session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, agent.Open{Cwd: t.TempDir()})
		if err != nil {
			t.Fatal(err)
		}
		if session.Capabilities().Steer != tc.works {
			t.Fatalf("features %s: Steer capability %v", tc.features, !tc.works)
		}
		r, err := session.Apply(ctx, agent.Operation{ID: "steer-1", Kind: agent.Steer, Text: "Also log the port."})
		if tc.works && (err != nil || r.Delivery != "submitted" || r.ID != "steer-1") {
			t.Fatalf("steer not submitted: %+v %v", r, err)
		}
		if !tc.works && !errors.Is(err, agent.ErrUnsupported) {
			t.Fatalf("an older worker was asked to steer: %v", err)
		}
		_ = session.Close()
	}
}

// A new worker is opened with the conversation's grants, as the worker
// reported them; taking one back reaches only a worker that holds grants.
func TestGrantsReachTheWorkerAtOpenAndCanBeRevoked(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	spec := `{"id":"g1","label":"x","tool":"Bash","exact":"make test"}`
	script := func(features string) string {
		return `read -r line
case "$line" in
  *'"grants":[` + spec + `]'*) printf '%s\n' '{"id":"lazybox-1","result":{"reference":"ref","features":` + features + `}}' ;;
  *) printf '%s\n' '{"id":"lazybox-1","error":{"message":"grants missing"}}' ;;
esac
while IFS= read -r line; do
  case "$line" in
    *'"method":"revoke"'*'"id":"g1"'*) printf '%s\n' '{"id":"lazybox-2","result":{"delivery":"acknowledged"}}' ;;
    *) printf '%s\n' '{"id":"lazybox-2","error":{"message":"unexpected"}}' ;;
  esac
done
`
	}
	grants := []agent.Grant{{ID: "g1", Label: "x", Tool: "Bash", Spec: []byte(spec)}}
	for _, tc := range []struct {
		features string
		works    bool
	}{{`["parts","grants"]`, true}, {`["parts"]`, false}} {
		worker := filepath.Join(t.TempDir(), "worker.sh")
		if err := os.WriteFile(worker, []byte(script(tc.features)), 0700); err != nil {
			t.Fatal(err)
		}
		session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, agent.Open{Cwd: t.TempDir(), Grants: grants})
		if err != nil {
			t.Fatalf("the worker was not opened with the grants: %v", err)
		}
		r, err := session.Apply(ctx, agent.Operation{ID: "revoke-1", Kind: agent.RevokeGrant, GrantID: "g1"})
		if tc.works && (err != nil || r.Delivery != "acknowledged" || r.ID != "revoke-1") {
			t.Fatalf("revoke not acknowledged: %+v %v", r, err)
		}
		if !tc.works && !errors.Is(err, agent.ErrUnsupported) {
			t.Fatalf("an older worker was asked to revoke: %v", err)
		}
		_ = session.Close()
	}
}

// A fork names the source session and the entry it keeps through; the worker
// answers with the new session. A worker that ignores the fork (one built
// before forks) would write into the source session: the open fails instead.
// A turn recorded without its entry names its last message instead.
func TestOpenForksTheSourceSessionIntoASeparateOne(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	for _, tc := range []struct {
		open               agent.Open
		want               string
		reference, failure string
	}{
		{agent.Open{ForkThread: "source", ForkAfterTurn: "entry"}, `*'"forkAfterTurn":"entry"'*'"forkThread":"source"'*`, "forked", ""},
		{agent.Open{ForkThread: "source", ForkAfterTurn: "entry"}, `*'"forkAfterTurn":"entry"'*'"forkThread":"source"'*`, "source", "separate conversation"},
		{agent.Open{ForkThread: "source", ForkAfterTurn: "entry"}, `*'"forkAfterTurn":"entry"'*'"forkThread":"source"'*`, "", "separate conversation"},
		{agent.Open{ForkThread: "source", ForkAfterMessage: "msg_last"}, `*'"forkAfterMessage":"msg_last"'*'"forkAfterTurn":""'*'"forkThread":"source"'*`, "forked", ""},
	} {
		worker := filepath.Join(t.TempDir(), "worker.sh")
		script := `read -r line
case "$line" in
  *'"method":"open"'` + tc.want + `) printf '%s\n' '{"id":"lazybox-1","result":{"reference":"` + tc.reference + `","features":[]}}' ;;
  *) printf '%s\n' '{"id":"lazybox-1","error":{"message":"fork missing"}}' ;;
esac
while IFS= read -r line; do :; done
`
		if err := os.WriteFile(worker, []byte(script), 0700); err != nil {
			t.Fatal(err)
		}
		tc.open.Cwd = t.TempDir()
		session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, tc.open)
		if tc.failure != "" {
			if err == nil || !strings.Contains(err.Error(), tc.failure) {
				t.Fatalf("worker answered %q: %v", tc.reference, err)
			}
			continue
		}
		if err != nil || session.Reference() != "forked" {
			t.Fatalf("fork %+v not opened: %v", tc.open, err)
		}
		_ = session.Close()
	}
}
