package accounts

import (
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// SignIn is what a look at a profile's credentials says, without starting
// its provider or reading a secret's value.
type SignIn string

const (
	// SignedIn: a credential is where the provider will look for one.
	SignedIn SignIn = "signed_in"
	// SignedOut: the provider would find no credential and ask to sign in.
	SignedOut SignIn = "signed_out"
	// SignInUnknown: the credential lives where it cannot be checked without
	// the provider (a keyring), so only using the profile tells.
	SignInUnknown SignIn = "unknown"
)

// claudeCredentialEnv are the variables through which Claude Code takes a
// credential or a cloud provider that supplies one.
var claudeCredentialEnv = []string{"ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"}

// CheckSignIn reports whether profile p has a credential its provider would
// use, resolving it exactly as an execution would (Resolve) against the
// Runner's environment. It reads only file presence and account metadata,
// never a token. The reason is one plain line for a person.
func CheckSignIn(p Profile, inherited []string) (SignIn, string) {
	resolved, err := Resolve(p, inherited)
	if err != nil {
		return SignedOut, err.Error()
	}
	env := map[string]string{}
	for _, entry := range resolved {
		if k, v, ok := strings.Cut(entry, "="); ok {
			env[k] = v
		}
	}
	if p.Provider == "codex" {
		return codexSignIn(env)
	}
	if ACPProvider(p.Provider) {
		// Each agent keeps its sign-in its own way; the first turn tells.
		return SignInUnknown, "Signs in with its own account"
	}
	return claudeSignIn(env)
}

func claudeSignIn(env map[string]string) (SignIn, string) {
	for _, name := range claudeCredentialEnv {
		if env[name] != "" {
			return SignedIn, "Signed in with a key or token from the environment"
		}
	}
	dir, err := nativeDirectory("claude", env)
	if err != nil {
		return SignInUnknown, "Could not find the Claude Code settings folder"
	}
	if nonEmpty(filepath.Join(dir, ".credentials.json")) {
		return SignedIn, "Signed in"
	}
	// The account Claude Code signed in with is recorded beside its settings:
	// in the config folder when one is set, else ~/.claude.json. The token
	// itself may be in the keychain.
	account := filepath.Join(dir, ".claude.json")
	if env["CLAUDE_CONFIG_DIR"] == "" {
		if home := env["HOME"]; home != "" {
			account = filepath.Join(home, ".claude.json")
		}
	}
	if email, ok := claudeAccount(account); ok {
		if email != "" {
			return SignedIn, "Signed in as " + email
		}
		return SignedIn, "Signed in"
	}
	if claudeSettingsCredential(filepath.Join(dir, "settings.json")) {
		return SignedIn, "Signed in through Claude Code settings"
	}
	return SignedOut, "Not signed in"
}

// claudeAccount reads the signed-in account's address from Claude Code's
// account file; ok is false when no account is recorded. The file also holds
// per-project history and can be large, so it is read only up to that key.
func claudeAccount(path string) (email string, ok bool) {
	f, err := os.Open(path)
	if err != nil {
		return "", false
	}
	defer f.Close()
	decoder := json.NewDecoder(io.LimitReader(f, 64<<20))
	if token, err := decoder.Token(); err != nil || token != json.Delim('{') {
		return "", false
	}
	for decoder.More() {
		key, err := decoder.Token()
		if err != nil {
			return "", false
		}
		if key != "oauthAccount" {
			var skip json.RawMessage
			if decoder.Decode(&skip) != nil {
				return "", false
			}
			continue
		}
		var account *struct {
			EmailAddress string `json:"emailAddress"`
		}
		if decoder.Decode(&account) != nil || account == nil {
			return "", false
		}
		return account.EmailAddress, true
	}
	return "", false
}

// claudeSettingsCredential reports a credential Claude Code's own settings
// supply: an API key helper, or a credential variable in its env block.
func claudeSettingsCredential(path string) bool {
	data, err := readSmall(path)
	if err != nil {
		return false
	}
	var settings struct {
		APIKeyHelper string            `json:"apiKeyHelper"`
		Env          map[string]string `json:"env"`
	}
	if json.Unmarshal(data, &settings) != nil {
		return false
	}
	if settings.APIKeyHelper != "" {
		return true
	}
	for _, name := range claudeCredentialEnv {
		if settings.Env[name] != "" {
			return true
		}
	}
	return false
}

func codexSignIn(env map[string]string) (SignIn, string) {
	if env["OPENAI_API_KEY"] != "" || env["CODEX_API_KEY"] != "" {
		return SignedIn, "Signed in with a key from the environment"
	}
	dir, err := nativeDirectory("codex", env)
	if err != nil {
		return SignInUnknown, "Could not find the Codex settings folder"
	}
	if nonEmpty(filepath.Join(dir, "auth.json")) {
		return SignedIn, "Signed in"
	}
	// Codex can keep its sign-in in the system keyring instead of auth.json.
	if config, err := readSmall(filepath.Join(dir, "config.toml")); err == nil {
		for _, line := range strings.Split(string(config), "\n") {
			key, value, ok := strings.Cut(line, "=")
			if ok && strings.TrimSpace(key) == "cli_auth_credentials_store" && !strings.Contains(value, "file") {
				return SignInUnknown, "Sign-in kept in the system keyring"
			}
		}
	}
	return SignedOut, "Not signed in"
}

func nonEmpty(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular() && info.Size() > 0
}

// readSmall reads a provider's metadata file, refusing one too large to be one.
func readSmall(path string) ([]byte, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > 4<<20 {
		return nil, os.ErrInvalid
	}
	return os.ReadFile(path)
}
