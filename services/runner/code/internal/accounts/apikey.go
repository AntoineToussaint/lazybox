package accounts

import (
	"errors"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// The Anthropic API key Claude agents run with (CLAUDE_COMPLIANCE.md): the
// way Anthropic asks products built on the Agent SDK to authenticate. The
// Runner has no Keychain storage, so the key is a 0600 file in a 0700 folder
// of its state, read when an agent starts and never returned on the wire.

// APIKeyFile is where a Runner keeps the key, under its state folder.
func APIKeyFile(state string) string {
	return filepath.Join(state, "secrets", "anthropic-api-key")
}

// ClaudeAuthEnv are the environment variables through which Claude Code finds
// a credential or a provider. An API key set by Lazybox replaces all of them,
// so the key is what the agent runs on, whatever the Runner inherited.
var ClaudeAuthEnv = []string{"ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_REFRESH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR", "CLAUDE_CODE_HOST_CREDS_FILE", "CLAUDE_CODE_SESSION_ACCESS_TOKEN", "CLAUDE_SESSION_INGRESS_TOKEN_FILE", "ANTHROPIC_PROFILE", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY", "CLAUDE_CODE_USE_ANTHROPIC_AWS", "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE", "CLAUDE_CODE_USE_MANTLE"}

// CheckAPIKey refuses what is not an Anthropic API key, a subscription token
// first: Lazybox may not store a Claude.ai credential.
func CheckAPIKey(key string) error {
	switch {
	case key == "":
		return errors.New("paste an Anthropic API key (it starts with sk-ant-)")
	case len(key) > 512 || strings.ContainsFunc(key, func(r rune) bool { return r <= ' ' || r == 0x7f }):
		return errors.New("that is not an Anthropic API key: it has spaces or is too long")
	case strings.HasPrefix(key, "sk-ant-oat"):
		return errors.New("that is a Claude subscription token, not an API key; Lazybox does not store subscription credentials (create a key at platform.claude.com)")
	case strings.HasPrefix(key, "sk-ant-admin"):
		return errors.New("that is an Admin API key; agents need an ordinary API key")
	case !strings.HasPrefix(key, "sk-ant-"):
		return errors.New("that is not an Anthropic API key (it starts with sk-ant-)")
	}
	return nil
}

// SaveAPIKey writes the key privately, replacing any earlier one.
func SaveAPIKey(state, key string) error {
	if err := CheckAPIKey(key); err != nil {
		return err
	}
	path := APIKeyFile(state)
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	if err := os.Chmod(dir, 0o700); err != nil {
		return err
	}
	f, err := os.CreateTemp(dir, ".key-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if err = f.Chmod(0o600); err == nil {
		_, err = f.WriteString(key)
	}
	if err == nil {
		err = f.Sync()
	}
	if err = errors.Join(err, f.Close()); err != nil {
		return err
	}
	return os.Rename(f.Name(), path)
}

// ForgetAPIKey removes the saved key; none saved is not an error.
func ForgetAPIKey(state string) error {
	if err := os.Remove(APIKeyFile(state)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

// LoadAPIKey reads the saved key: "" when there is none. A file another user
// could read, or one that is not a key, is an error rather than a key used.
func LoadAPIKey(state string) (string, error) {
	path := APIKeyFile(state)
	info, err := os.Lstat(path)
	if errors.Is(err, os.ErrNotExist) {
		return "", nil
	}
	if err != nil {
		return "", err
	}
	if !info.Mode().IsRegular() || info.Mode().Perm()&0o077 != 0 || info.Size() > 512 {
		return "", errors.New("the saved Anthropic API key must be a private file (0600); save it again in Settings")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	key := strings.TrimSpace(string(data))
	if err = CheckAPIKey(key); err != nil {
		return "", errors.New("the saved Anthropic API key is not valid; save it again in Settings")
	}
	return key, nil
}

// APIKeyHint is the last four characters of a key, to recognise it by.
func APIKeyHint(key string) string {
	if len(key) < 12 {
		return ""
	}
	return key[len(key)-4:]
}

// WithAPIKey is env with key as the only Claude credential: every other
// credential or provider selection is removed and ANTHROPIC_API_KEY set.
func WithAPIKey(env []string, key string) []string {
	out := make([]string, 0, len(env)+1)
	for _, entry := range env {
		name, _, _ := strings.Cut(entry, "=")
		if !contains(ClaudeAuthEnv, name) {
			out = append(out, entry)
		}
	}
	out = append(out, "ANTHROPIC_API_KEY="+key)
	sort.Strings(out)
	return out
}

// CloudProvider is the cloud provider env selects for Claude Code, or "".
func CloudProvider(env []string) string {
	for _, entry := range env {
		name, value, _ := strings.Cut(entry, "=")
		if value == "" || value == "0" || strings.EqualFold(value, "false") {
			continue
		}
		switch name {
		case "CLAUDE_CODE_USE_BEDROCK":
			return "bedrock"
		case "CLAUDE_CODE_USE_VERTEX":
			return "vertex"
		case "CLAUDE_CODE_USE_FOUNDRY":
			return "foundry"
		case "CLAUDE_CODE_USE_ANTHROPIC_AWS":
			return "anthropic-aws"
		}
	}
	return ""
}

// HasAPICredential reports whether env gives Claude Code an API credential or
// a cloud provider, rather than leaving it to a subscription sign-in.
func HasAPICredential(env []string) bool {
	if CloudProvider(env) != "" {
		return true
	}
	for _, entry := range env {
		name, value, _ := strings.Cut(entry, "=")
		if (name == "ANTHROPIC_API_KEY" || name == "ANTHROPIC_AUTH_TOKEN") && value != "" {
			return true
		}
	}
	return false
}

func contains(list []string, value string) bool {
	for _, v := range list {
		if v == value {
			return true
		}
	}
	return false
}
