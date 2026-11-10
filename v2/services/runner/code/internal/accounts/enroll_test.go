package accounts

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestClaudeEnrollmentRetainsOnePrivateTokenWithoutChangingCurrentLogin(t *testing.T) {
	t.Setenv("CLAUDE_CODE_OAUTH_TOKEN", "current-login")
	root := t.TempDir()
	t.Setenv("CLAUDE_CONFIG_DIR", t.TempDir())
	p, err := Enroll(context.Background(), root, "claude-work", "claude", "", strings.NewReader("fixture-subscription-token\n"), nil, io.Discard, io.Discard)
	if err != nil {
		t.Fatal(err)
	}
	profiles, err := LoadProfiles(root)
	if err != nil || len(profiles) != 1 || profiles[0].ID != p.ID {
		t.Fatal(profiles, err)
	}
	info, err := os.Stat(p.TokenFile)
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatal("credential file is not private", err)
	}
	// Resolve it repeatedly, as successive native sessions after rotation do.
	for i := 0; i < 3; i++ {
		env, err := Resolve(p, []string{"CLAUDE_CODE_OAUTH_TOKEN=current-login", "ANTHROPIC_API_KEY=other-billing", "CLAUDE_CONFIG_DIR=/existing-permissions"})
		if err != nil {
			t.Fatal(err)
		}
		joined := strings.Join(env, "\n")
		if !strings.Contains(joined, "CLAUDE_CODE_OAUTH_TOKEN=fixture-subscription-token") || strings.Contains(joined, "other-billing") || !strings.Contains(joined, "CLAUDE_CONFIG_DIR="+p.Home) || strings.Contains(joined, "/existing-permissions") {
			t.Fatal("wrong credential or settings context")
		}
	}
	if os.Getenv("CLAUDE_CODE_OAUTH_TOKEN") != "current-login" {
		t.Fatal("global login changed")
	}
	if _, err = Enroll(context.Background(), root, p.ID, "claude", "", strings.NewReader("replacement"), nil, io.Discard, io.Discard); err == nil {
		t.Fatal("existing profile overwritten")
	}
	metadata, _ := os.ReadFile(filepath.Join(root, "accounts", "profiles.json"))
	if strings.Contains(string(metadata), "fixture-subscription-token") {
		t.Fatal("token in profile metadata")
	}
}
func TestEnrollmentAndResolutionRejectUnsafeProfiles(t *testing.T) {
	root := t.TempDir()
	for _, id := range []string{"../global", "", "a/b"} {
		if _, err := Enroll(context.Background(), root, id, "claude", "", strings.NewReader("token"), nil, io.Discard, io.Discard); err == nil {
			t.Fatal(id)
		}
	}
	if _, err := Enroll(context.Background(), root, "too-big", "claude", "", strings.NewReader(strings.Repeat("x", 16385)), nil, io.Discard, io.Discard); err == nil {
		t.Fatal("oversized secret")
	}
	if _, err := os.Stat(filepath.Join(root, "accounts", "too-big")); !os.IsNotExist(err) {
		t.Fatal("failed enrollment retained account")
	}
	path := filepath.Join(root, "public-token")
	if err := os.WriteFile(path, []byte("secret"), 0644); err != nil {
		t.Fatal(err)
	}
	if _, err := Resolve(Profile{ID: "unsafe", Provider: "claude", Isolation: "environment", TokenFile: path}, nil); err == nil {
		t.Fatal("public credential file accepted")
	}
}
func TestCodexEnrollmentPinsNativeLoginToItsOwnHome(t *testing.T) {
	if os.PathSeparator != '/' {
		t.Skip("shell fixture")
	}
	root := t.TempDir()
	binary := filepath.Join(t.TempDir(), "codex-fixture")
	// Synthetic CLI checks argv and CODEX_HOME. No browser or provider is contacted.
	script := `#!/bin/sh
[ "$1" = '-c' ] && [ "$2" = 'cli_auth_credentials_store="file"' ] && [ "$3" = 'login' ] || exit 11
[ -n "$CODEX_HOME" ] && [ -z "$OPENAI_API_KEY" ] || exit 12
printf '%s' 'fixture-refreshable-auth' > "$CODEX_HOME/auth.json"
`
	if err := os.WriteFile(binary, []byte(script), 0700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("OPENAI_API_KEY", "global-key")
	t.Setenv("CODEX_HOME", "/global/profile")
	p, err := Enroll(context.Background(), root, "codex-work", "codex", binary, nil, nil, io.Discard, io.Discard)
	if err != nil {
		t.Fatal(err)
	}
	if p.Home == "/global/profile" || os.Getenv("CODEX_HOME") != "/global/profile" {
		t.Fatal("global home changed")
	}
	if _, err = os.Stat(filepath.Join(p.Home, "auth.json")); err != nil {
		t.Fatal(err)
	}
}
