package accounts

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func write(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestCheckSignInFindsClaudeCredentialsWhereClaudeCodeKeepsThem(t *testing.T) {
	inherited := Profile{ID: "claude-default", Provider: "claude", Isolation: "inherited"}
	for _, c := range []struct {
		name   string
		setup  func(home string) []string
		want   SignIn
		reason string
	}{
		{"nothing", func(home string) []string { return nil }, SignedOut, "Not signed in"},
		{"credentials file", func(home string) []string {
			write(t, filepath.Join(home, ".claude", ".credentials.json"), `{"claudeAiOauth":{}}`)
			return nil
		}, SignedIn, "Signed in"},
		{"keychain account", func(home string) []string {
			write(t, filepath.Join(home, ".claude.json"), `{"numStartups":3,"projects":{"/a":{"history":[1,2,3]}},"oauthAccount":{"emailAddress":"me@example.invalid"}}`)
			return nil
		}, SignedIn, "Signed in as me@example.invalid"},
		{"signed out account file", func(home string) []string {
			write(t, filepath.Join(home, ".claude.json"), `{"numStartups":3,"oauthAccount":null}`)
			return nil
		}, SignedOut, "Not signed in"},
		{"api key", func(home string) []string { return []string{"ANTHROPIC_API_KEY=sk-test"} }, SignedIn, "Signed in with a key or token from the environment"},
		{"api key helper", func(home string) []string {
			write(t, filepath.Join(home, ".claude", "settings.json"), `{"apiKeyHelper":"/bin/echo key"}`)
			return nil
		}, SignedIn, "Signed in through Claude Code settings"},
		{"config dir", func(home string) []string {
			dir := filepath.Join(home, "elsewhere")
			write(t, filepath.Join(dir, ".claude.json"), `{"oauthAccount":{"emailAddress":""}}`)
			return []string{"CLAUDE_CONFIG_DIR=" + dir}
		}, SignedIn, "Signed in"},
	} {
		t.Run(c.name, func(t *testing.T) {
			home := t.TempDir()
			env := append([]string{"HOME=" + home}, c.setup(home)...)
			got, reason := CheckSignIn(inherited, env)
			if got != c.want || reason != c.reason {
				t.Fatalf("got %s %q, want %s %q", got, reason, c.want, c.reason)
			}
		})
	}
}

func TestCheckSignInFindsCodexCredentials(t *testing.T) {
	inherited := Profile{ID: "codex-default", Provider: "codex", Isolation: "inherited"}
	home := t.TempDir()
	env := []string{"HOME=" + home}
	if got, _ := CheckSignIn(inherited, env); got != SignedOut {
		t.Fatalf("no auth.json: got %s", got)
	}
	write(t, filepath.Join(home, ".codex", "config.toml"), "model = \"gpt-5\"\ncli_auth_credentials_store = \"keyring\"\n")
	if got, reason := CheckSignIn(inherited, env); got != SignInUnknown || !strings.Contains(reason, "keyring") {
		t.Fatalf("keyring: got %s %q", got, reason)
	}
	write(t, filepath.Join(home, ".codex", "auth.json"), `{"tokens":{}}`)
	if got, _ := CheckSignIn(inherited, env); got != SignedIn {
		t.Fatalf("auth.json: got %s", got)
	}
	if got, _ := CheckSignIn(inherited, []string{"HOME=" + t.TempDir(), "OPENAI_API_KEY=sk-test"}); got != SignedIn {
		t.Fatalf("api key: got %s", got)
	}
}

// A saved subscription is checked in its own folder, never the CLI's: the
// person's own sign-in does not make a private subscription look signed in.
func TestCheckSignInReadsAManagedSubscriptionsOwnFolder(t *testing.T) {
	home, state := t.TempDir(), t.TempDir()
	write(t, filepath.Join(home, ".codex", "auth.json"), `{"tokens":{}}`)
	saved := Profile{ID: "codex-work", Provider: "codex", Isolation: "environment", Managed: true, Home: filepath.Join(state, "accounts", "codex-work", "codex")}
	env := []string{"HOME=" + home}
	if got, _ := CheckSignIn(saved, env); got != SignedOut {
		t.Fatalf("empty private folder: got %s", got)
	}
	write(t, filepath.Join(saved.Home, "auth.json"), `{"tokens":{}}`)
	if got, _ := CheckSignIn(saved, env); got != SignedIn {
		t.Fatalf("private auth.json: got %s", got)
	}
	// A profile that cannot resolve says why, as a sign-in to fix.
	broken := Profile{ID: "claude-work", Provider: "claude", Isolation: "environment", Home: filepath.Join(state, "accounts", "claude-work", "claude")}
	if got, reason := CheckSignIn(broken, env); got != SignedOut || reason == "" {
		t.Fatalf("unresolvable: got %s %q", got, reason)
	}
}
