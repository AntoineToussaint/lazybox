package accounts

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"

	"github.com/tailscale/hujson"
)

func nativeDirectory(provider string, env map[string]string) (string, error) {
	name, suffix := "CODEX_HOME", ".codex"
	if provider == "claude" {
		name, suffix = "CLAUDE_CONFIG_DIR", ".claude"
	}
	if value := env[name]; value != "" {
		return filepath.Abs(value)
	}
	home := env["HOME"]
	if home == "" {
		var err error
		home, err = os.UserHomeDir()
		if err != nil {
			return "", err
		}
	}
	return filepath.Join(home, suffix), nil
}
func resolvedDirectory(path string) string {
	if resolved, err := filepath.EvalSymlinks(path); err == nil {
		return resolved
	}
	return filepath.Clean(path)
}
func rejectSharedDirectory(p Profile, env map[string]string) error {
	if p.Home == "" || !filepath.IsAbs(p.Home) {
		return errors.New("isolated profiles require an absolute private provider directory")
	}
	current, err := nativeDirectory(p.Provider, env)
	if err != nil {
		return err
	}
	defaultDirectory, err := nativeDirectory(p.Provider, map[string]string{"HOME": env["HOME"]})
	if err != nil {
		return err
	}
	for _, local := range []string{current, defaultDirectory} {
		if resolvedDirectory(local) == resolvedDirectory(p.Home) {
			return errors.New("isolated profile cannot use a local CLI authentication directory")
		}
	}
	return nil
}

// Seed only permission preferences into the new account's own user-settings
// tier. Authentication, env, credential helpers and account metadata are never
// copied. Project/local and managed policy remain native provider layers.
func seedClaudePermissions(target string) error {
	inherited := map[string]string{"HOME": os.Getenv("HOME"), "CLAUDE_CONFIG_DIR": os.Getenv("CLAUDE_CONFIG_DIR")}
	current, err := nativeDirectory("claude", inherited)
	if err != nil {
		return err
	}
	data, err := os.ReadFile(filepath.Join(current, "settings.json"))
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return errors.New("cannot read existing Claude permission preferences")
	}
	var settings map[string]json.RawMessage
	data, err = hujson.Standardize(data)
	if err != nil || json.Unmarshal(data, &settings) != nil {
		return errors.New("existing Claude settings are invalid; permissions were not replaced")
	}
	permissionSettings := map[string]json.RawMessage{}
	for _, key := range []string{"permissions", "sandbox"} {
		if value := settings[key]; len(value) > 0 {
			permissionSettings[key] = value
		}
	}
	if len(permissionSettings) == 0 {
		return nil
	}
	encoded, err := json.Marshal(permissionSettings)
	if err != nil {
		return err
	}
	file, err := os.OpenFile(filepath.Join(target, "settings.json"), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return err
	}
	_, err = file.Write(encoded)
	if err == nil {
		err = file.Sync()
	}
	return errors.Join(err, file.Close())
}
