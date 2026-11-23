package runner

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

type agentEndpointFile struct {
	URL       string `json:"url"`
	Token     string `json:"token"`
	SessionID string `json:"sessionId"`
}

func readAgentEndpoint(t *testing.T, path string) agentEndpointFile {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("agent endpoint mode %v, want 0600", info.Mode().Perm())
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var f agentEndpointFile
	if err = json.Unmarshal(data, &f); err != nil {
		t.Fatal(err)
	}
	return f
}

func envValue(env []string, key string) (string, int) {
	value, count := "", 0
	for _, entry := range env {
		if v, ok := strings.CutPrefix(entry, key+"="); ok {
			value, count = v, count+1
		}
	}
	return value, count
}

// The owner's endpoint file holds the bearer that can approve, answer and
// delete. No agent or managed command may be pointed at it, even when the
// Runner itself was started from a shell that exports it. Nor do they inherit
// the Runner's Codefly service wiring: its credentials, its endpoints and its
// container recovery marker, which made every `codefly` an agent ran refuse
// to start ("container recovery marker does not belong to this process").
func TestAgentsAndCommandsNeverSeeTheOwnerEndpoint(t *testing.T) {
	owner := "/owner/state/endpoint.json"
	t.Setenv("LAZYBOX_V2_ENDPOINT", owner)
	t.Setenv("LAZYBOX_V2_STATE", "/owner/state")
	t.Setenv("CODEFLY_CONTAINER_RECOVERY_SCOPE", "4242:v2:runner:ns")
	t.Setenv("CODEFLY_AGENT_TOKEN", "runner-codefly-secret")
	t.Setenv("CODEFLY_PROCESS_GROUP_AUTH", "runner-group-secret")
	t.Setenv("CODEFLY__ENDPOINT__LAZYBOX_V2__RUNNER__CONNECT__CONNECT", "http://localhost:1")
	state := t.TempDir()
	b, err := New(Config{State: state, OutputFallbackRoot: t.TempDir(), Address: "http://127.0.0.1:1"})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	for name, env := range map[string][]string{"agent": fake.Session.Open.Env, "execution": e.Env} {
		for _, entry := range env {
			if strings.Contains(entry, owner) || strings.Contains(entry, filepath.Join(state, "endpoint.json")) || strings.HasPrefix(entry, "LAZYBOX_V2_STATE=") {
				t.Fatalf("%s env exposes the owner: %q", name, entry)
			}
			if strings.HasPrefix(entry, "CODEFLY") {
				t.Fatalf("%s env inherits the Runner's Codefly wiring: %q", name, entry)
			}
		}
		path, n := envValue(env, "LAZYBOX_V2_ENDPOINT")
		if n != 1 || path != b.AgentEndpoint(e.ID) {
			t.Fatalf("%s env endpoint %q (%d entries), want %q", name, path, n, b.AgentEndpoint(e.ID))
		}
	}
	f := readAgentEndpoint(t, b.AgentEndpoint(e.ID))
	if f.URL != "http://127.0.0.1:1" || f.SessionID != e.ID || f.Token == "" {
		t.Fatalf("agent endpoint %+v", f)
	}
	if session, err := b.AgentSession(context.Background(), f.Token); err != nil || session != e.ID {
		t.Fatal("agent credential not bound to its execution", session, err)
	}

	// A managed command gets the same scoped endpoint, never the owner's.
	c, err := b.Start(e, "env", []string{"/usr/bin/env"})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if _, err = c.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	out, _, err := c.Read(ctx, 0, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	printed := strings.Split(string(out), "\n")
	if path, n := envValue(printed, "LAZYBOX_V2_ENDPOINT"); n != 1 || path != b.AgentEndpoint(e.ID) {
		t.Fatalf("managed command endpoint %q (%d)", path, n)
	}
	if strings.Contains(string(out), owner) || strings.Contains(string(out), f.Token) {
		t.Fatal("managed command env exposes the owner endpoint or a credential")
	}
	if strings.Contains(string(out), "CODEFLY") {
		t.Fatal("managed command env inherits the Runner's Codefly wiring")
	}
	// Account probes run the person's provider CLIs with the same environment.
	probe, err := b.profileEnv(accounts.Profile{ID: "codex-default", Provider: "codex", Isolation: "inherited"})
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range probe {
		if strings.HasPrefix(entry, "CODEFLY") || strings.HasPrefix(entry, "LAZYBOX_V2_") {
			t.Fatalf("an account probe inherits %q", entry)
		}
	}
}

// Credentials derive from a key kept in the Runner state, so an agent that
// outlives its Runner keeps publishing to the next one. Deleting the
// conversation revokes it.
func TestAgentCredentialSurvivesARunnerRestartAndDiesWithItsConversation(t *testing.T) {
	state := t.TempDir()
	first, err := New(Config{State: state, OutputFallbackRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	first.Adapters["codex"] = &agenttest.Adapter{}
	e, err := first.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	other, err := first.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	token := readAgentEndpoint(t, first.AgentEndpoint(e.ID)).Token
	if err = first.Close(); err != nil {
		t.Fatal(err)
	}
	key, err := os.Stat(filepath.Join(state, "agent-credential.key"))
	if err != nil || key.Mode().Perm() != 0o600 {
		t.Fatal("credential key not private", err)
	}

	second, err := New(Config{State: state, OutputFallbackRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	ctx := context.Background()
	if session, err := second.AgentSession(ctx, token); err != nil || session != e.ID {
		t.Fatal("credential lost across a Runner restart", session, err)
	}
	if err = second.DeleteConversation(ctx, e.ID, true); err != nil {
		t.Fatal(err)
	}
	if _, err = second.AgentSession(ctx, token); err == nil {
		t.Fatal("deleted conversation's credential still valid")
	}
	if _, err = os.Stat(second.AgentEndpoint(e.ID)); !os.IsNotExist(err) {
		t.Fatal("deleted conversation's endpoint file kept", err)
	}
	if _, err = second.AgentSession(ctx, readAgentEndpoint(t, second.AgentEndpoint(other.ID)).Token); err != nil {
		t.Fatal("deleting one conversation revoked another", err)
	}
}

// A live agent re-attached by the next Runner reads the same file and finds
// that Runner's address with a credential it still accepts.
func TestAdoptionPointsALiveAgentsEndpointAtTheNewRunner(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	first := fixtureBench(t, state)
	first.Config.Address = "http://old.invalid"
	e, err := openAwaitingApproval(first)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, first, e.ID))
	path := first.AgentEndpoint(e.ID)
	before := readAgentEndpoint(t, path)
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	second := fixtureBench(t, state)
	second.Config.Address = "http://new.invalid"
	if err = second.Adopt(context.Background()); err != nil {
		t.Fatal(err)
	}
	after := readAgentEndpoint(t, path)
	if after.URL != "http://new.invalid" || after.Token != before.Token {
		t.Fatalf("adopted endpoint %+v, was %+v", after, before)
	}
	if session, err := second.AgentSession(context.Background(), after.Token); err != nil || session != e.ID {
		t.Fatal("adopted agent's credential refused", err)
	}
	if err = second.Close(); err != nil {
		t.Fatal(err)
	}
	stoppedEventually(t, pid)
}
