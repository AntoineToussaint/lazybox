package accounts

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// A fixture key: shaped like an Anthropic API key, valid nowhere.
const fixtureKey = "sk-ant-api03-fixture-not-a-real-key-0000-wxyz"

func TestAPIKeyIsKeptPrivateAndNeverASubscriptionToken(t *testing.T) {
	state := t.TempDir()
	if key, err := LoadAPIKey(state); err != nil || key != "" {
		t.Fatalf("no key saved: %q %v", key, err)
	}
	for _, bad := range []string{"", "sk-ant-oat01-a-subscription-token", "sk-ant-admin01-x", "not-a-key", "sk-ant-api03 with space", "sk-ant-" + strings.Repeat("x", 600)} {
		if err := SaveAPIKey(state, bad); err == nil {
			t.Fatalf("saved %q", bad)
		}
	}
	if _, err := os.Stat(APIKeyFile(state)); !os.IsNotExist(err) {
		t.Fatal("a refused key left a file")
	}
	if err := SaveAPIKey(state, fixtureKey); err != nil {
		t.Fatal(err)
	}
	info, err := os.Stat(APIKeyFile(state))
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("key file mode %v %v", info.Mode(), err)
	}
	if dir, _ := os.Stat(filepath.Dir(APIKeyFile(state))); dir.Mode().Perm() != 0o700 {
		t.Fatalf("secrets folder mode %v", dir.Mode())
	}
	if key, err := LoadAPIKey(state); err != nil || key != fixtureKey {
		t.Fatalf("loaded %q %v", key, err)
	}
	if APIKeyHint(fixtureKey) != "wxyz" {
		t.Fatal("hint is the last four characters")
	}
	// A file another user could read is not used.
	if err = os.Chmod(APIKeyFile(state), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadAPIKey(state); err == nil {
		t.Fatal("a world-readable key file was used")
	}
	if err = ForgetAPIKey(state); err != nil {
		t.Fatal(err)
	}
	if err = ForgetAPIKey(state); err != nil {
		t.Fatal("forgetting twice is not an error")
	}
	if key, _ := LoadAPIKey(state); key != "" {
		t.Fatal("key still there")
	}
}

func TestWithAPIKeyIsTheOnlyClaudeCredential(t *testing.T) {
	env := WithAPIKey([]string{"PATH=/bin", "CLAUDE_CODE_OAUTH_TOKEN=sub", "ANTHROPIC_API_KEY=old", "ANTHROPIC_AUTH_TOKEN=tok", "CLAUDE_CODE_USE_BEDROCK=1", "CLAUDE_CONFIG_DIR=/x"}, fixtureKey)
	want := []string{"ANTHROPIC_API_KEY=" + fixtureKey, "CLAUDE_CONFIG_DIR=/x", "PATH=/bin"}
	if !slices.Equal(env, want) {
		t.Fatalf("env %v, want %v", env, want)
	}
	if !HasAPICredential(env) || HasAPICredential([]string{"CLAUDE_CODE_OAUTH_TOKEN=sub"}) {
		t.Fatal("API credential detection")
	}
	for value, want := range map[string]string{"CLAUDE_CODE_USE_BEDROCK=1": "bedrock", "CLAUDE_CODE_USE_VERTEX=true": "vertex", "CLAUDE_CODE_USE_FOUNDRY=1": "foundry", "CLAUDE_CODE_USE_ANTHROPIC_AWS=1": "anthropic-aws", "CLAUDE_CODE_USE_BEDROCK=0": ""} {
		if got := CloudProvider([]string{value}); got != want {
			t.Fatalf("%s: %q, want %q", value, got, want)
		}
	}
}

func TestPolicyPoolKeepsItsOrderAndOnlySignedInAccounts(t *testing.T) {
	profiles := []Profile{{ID: "claude-default", Provider: "claude", Isolation: "inherited"}, {ID: "work", Provider: "claude", Isolation: "environment"}, {ID: "home", Provider: "claude", Isolation: "environment"}, {ID: "spare", Provider: "claude", Isolation: "environment"}, {ID: "codex-work", Provider: "codex", Isolation: "environment"}}
	selected := profiles[0]
	if got := (Policy{}).Pool(profiles, selected, []string{"spare"}); !slices.Equal(got, []string{"home", "work"}) {
		t.Fatalf("default pool %v", got)
	}
	if got := (Policy{Accounts: []string{"spare", "codex-work", "missing", "work", "claude-default", "home"}}).Pool(profiles, selected, []string{"home"}); !slices.Equal(got, []string{"spare", "work"}) {
		t.Fatalf("ordered pool %v: its order, this provider's signed-in accounts only, nothing excluded", got)
	}
}
