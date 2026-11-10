package accounts

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestManagedAccountsCannotOverwriteLocalAuthenticationOrCopyCredentials(t *testing.T) {
	local := t.TempDir()
	claudeDir := filepath.Join(local, ".claude")
	codexDir := filepath.Join(local, ".codex")
	for _, dir := range []string{claudeDir, codexDir} {
		if err := os.Mkdir(dir, 0700); err != nil {
			t.Fatal(err)
		}
	}
	t.Setenv("HOME", local)
	t.Setenv("CLAUDE_CONFIG_DIR", claudeDir)
	t.Setenv("CODEX_HOME", codexDir)
	t.Setenv("CLAUDE_CODE_OAUTH_TOKEN", "local-claude-token")
	t.Setenv("CLAUDE_CODE_OAUTH_REFRESH_TOKEN", "local-claude-refresh")
	t.Setenv("OPENAI_API_KEY", "local-openai-key")
	fixtures := map[string]string{
		filepath.Join(local, ".claude.json"):          `{"oauthAccount":{"accountUuid":"original-account"}}`,
		filepath.Join(claudeDir, ".credentials.json"): `{"claudeAiOauth":{"accessToken":"local-claude-secret","refreshToken":"local-refresh-secret"}}`,
		filepath.Join(codexDir, "auth.json"):          `{"tokens":{"access_token":"local-codex-secret"}}`,
		filepath.Join(codexDir, "config.toml"):        `approval_policy = "on-request"`,
		filepath.Join(claudeDir, "settings.json"):     "{\n// Existing permission settings must retain their meaning.\n\"permissions\":{\"defaultMode\":\"bypassPermissions\",\"deny\":[\"Bash(rm*)\"],},\n\"sandbox\":{\"enabled\":true},\n\"env\":{\"ANTHROPIC_API_KEY\":\"local-api-key\"},\n\"apiKeyHelper\":\"local-credential-helper\",\n}",
	}
	before := map[string][32]byte{}
	for path, value := range fixtures {
		if err := os.WriteFile(path, []byte(value), 0600); err != nil {
			t.Fatal(err)
		}
		before[path] = sha256.Sum256([]byte(value))
	}
	state := t.TempDir()
	claude, err := Enroll(context.Background(), state, "claude-work", "claude", "", strings.NewReader("private-managed-token"), nil, io.Discard, io.Discard)
	if err != nil {
		t.Fatal(err)
	}
	if !claude.Managed || !strings.HasPrefix(claude.Home, state+string(os.PathSeparator)) || claude.Home == claudeDir {
		t.Fatal("shared Claude configuration directory")
	}
	permissionData, err := os.ReadFile(filepath.Join(claude.Home, "settings.json"))
	if err != nil {
		t.Fatal(err)
	}
	var copied map[string]any
	if err = json.Unmarshal(permissionData, &copied); err != nil {
		t.Fatal(err)
	}
	if len(copied) != 2 || !reflect.DeepEqual(copied["permissions"], map[string]any{"defaultMode": "bypassPermissions", "deny": []any{"Bash(rm*)"}}) || copied["sandbox"] == nil {
		t.Fatalf("permissions were lost or auth was copied: %s", permissionData)
	}
	for _, secret := range []string{"local-api-key", "apiKeyHelper", "local-claude-secret", "local-codex-secret", "original-account"} {
		if strings.Contains(string(permissionData), secret) {
			t.Fatal("local auth data copied")
		}
	}
	env, err := Resolve(claude, os.Environ())
	if err != nil {
		t.Fatal(err)
	}
	joined := strings.Join(env, "\n")
	if !strings.Contains(joined, "CLAUDE_CONFIG_DIR="+claude.Home) || !strings.Contains(joined, "CLAUDE_CODE_OAUTH_TOKEN=private-managed-token") || strings.Contains(joined, "CLAUDE_CODE_OAUTH_REFRESH_TOKEN=") || strings.Contains(joined, "CLAUDE_CODE_OAUTH_TOKEN=local-") {
		t.Fatal("managed Claude inherited local authentication")
	}
	// The native login fixture writes an auth file, just as the provider does,
	// and refuses any credential context except the newly created private home.
	binary := filepath.Join(t.TempDir(), "codex-fixture")
	script := `#!/bin/sh
[ "$1" = '-c' ] && [ "$2" = 'cli_auth_credentials_store="file"' ] && [ "$3" = 'login' ] || exit 11
[ -n "$CODEX_HOME" ] && [ -z "$OPENAI_API_KEY" ] || exit 12
printf '%s' 'managed-codex-auth' > "$CODEX_HOME/auth.json"
`
	if err = os.WriteFile(binary, []byte(script), 0700); err != nil {
		t.Fatal(err)
	}
	codex, err := Enroll(context.Background(), state, "codex-work", "codex", binary, nil, nil, io.Discard, io.Discard)
	if err != nil {
		t.Fatal(err)
	}
	privateAuth, err := os.ReadFile(filepath.Join(codex.Home, "auth.json"))
	if err != nil || string(privateAuth) != "managed-codex-auth" {
		t.Fatal("native login did not use private account storage", err)
	}
	registered, err := LoadProfiles(state)
	if err != nil || len(registered) != 2 || !registered[0].Managed || !registered[1].Managed {
		t.Fatal("enrollment ownership was not restored", err)
	}
	for path, hash := range before {
		data, err := os.ReadFile(path)
		if err != nil || sha256.Sum256(data) != hash {
			t.Fatalf("local auth/config changed: %s %v", path, err)
		}
		info, err := os.Stat(path)
		if err != nil || info.Mode().Perm() != 0600 {
			t.Fatalf("local auth/config permissions changed: %s %v", path, err)
		}
	}
	if os.Getenv("CLAUDE_CONFIG_DIR") != claudeDir || os.Getenv("CODEX_HOME") != codexDir || os.Getenv("CLAUDE_CODE_OAUTH_TOKEN") != "local-claude-token" {
		t.Fatal("process-wide authentication context changed")
	}
}
func TestIsolationRejectsCurrentAuthenticationDirectoryAndSymlinks(t *testing.T) {
	for _, provider := range []string{"codex", "claude"} {
		current := t.TempDir()
		name := "CODEX_HOME"
		if provider == "claude" {
			name = "CLAUDE_CONFIG_DIR"
		}
		env := []string{name + "=" + current}
		profile := Profile{ID: "unsafe", Provider: provider, Isolation: "environment", Home: current}
		if _, err := Resolve(profile, env); err == nil {
			t.Fatal("current auth directory accepted")
		}
		link := filepath.Join(t.TempDir(), "alias")
		if err := os.Symlink(current, link); err != nil {
			t.Fatal(err)
		}
		profile.Home = link
		if _, err := Resolve(profile, env); err == nil {
			t.Fatal("symlink to current auth directory accepted")
		}
		// A custom current directory must not make the default local store writable.
		home := t.TempDir()
		defaultDir := filepath.Join(home, "."+provider)
		if err := os.Mkdir(defaultDir, 0700); err != nil {
			t.Fatal(err)
		}
		profile.Home = defaultDir
		if _, err := Resolve(profile, append(env, "HOME="+home)); err == nil {
			t.Fatal("default local auth directory accepted")
		}
	}
}

func TestNativeClaudeEnrollmentRemainsPrivateAfterReload(t *testing.T) {
	local := t.TempDir()
	t.Setenv("CLAUDE_CONFIG_DIR", local)
	original := []byte(`{"permissions":{"defaultMode":"plan"},"env":{"ANTHROPIC_API_KEY":"local-secret"}}`)
	if err := os.WriteFile(filepath.Join(local, "settings.json"), original, 0600); err != nil {
		t.Fatal(err)
	}
	state := t.TempDir()
	p, err := PrepareLogin(state, "personal", "claude", "Personal")
	if err != nil {
		t.Fatal(err)
	}
	if !p.NativeLogin || !p.Managed || p.TokenFile != "" {
		t.Fatal("native credential mode not selected")
	}
	if _, err = PrepareLogin(state, "personal", "claude", "Replacement"); err == nil {
		t.Fatal("existing account overwritten")
	}
	if err = StoreProfile(state, p); err != nil {
		t.Fatal(err)
	}
	saved, err := LoadProfiles(state)
	if err != nil || len(saved) != 1 {
		t.Fatal(err)
	}
	env, err := Resolve(saved[0], []string{"CLAUDE_CONFIG_DIR=" + local, "CLAUDE_CODE_OAUTH_TOKEN=local-token", "ANTHROPIC_API_KEY=local-key"})
	if err != nil {
		t.Fatal(err)
	}
	joined := strings.Join(env, "\n")
	if strings.Contains(joined, "local-token") || strings.Contains(joined, "local-key") || !strings.Contains(joined, "CLAUDE_CONFIG_DIR="+p.Home) {
		t.Fatal("native private login inherited local credentials")
	}
	bytes, err := os.ReadFile(filepath.Join(local, "settings.json"))
	if err != nil || string(bytes) != string(original) {
		t.Fatal("existing permissions changed")
	}
	p.Managed = false
	if _, err = Resolve(p, nil); err == nil {
		t.Fatal("unmanaged profile accepted native auth fallback")
	}
	for _, id := range []string{"../escape", "", "a/b"} {
		if _, err = PrepareLogin(state, id, "claude", "Invalid"); err == nil {
			t.Fatal("invalid profile identity accepted")
		}
	}
}
