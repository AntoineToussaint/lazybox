package accounts

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"syscall"
)

var profileID = regexp.MustCompile(`^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$`)

// LoadProfiles reads Runner-owned enrollment metadata, never credentials.
func LoadProfiles(state string) ([]Profile, error) {
	data, err := os.ReadFile(filepath.Join(state, "accounts", "profiles.json"))
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var profiles []Profile
	err = json.Unmarshal(data, &profiles)
	for i := range profiles {
		profiles[i].Managed = true
	}
	return profiles, err
}

// ErrClaudeTokenImport refuses a Claude subscription token: Lazybox does not
// store Claude.ai credentials (CLAUDE_COMPLIANCE.md).
var ErrClaudeTokenImport = errors.New("Lazybox does not store Claude subscription tokens (Anthropic's terms do not allow it; see CLAUDE_COMPLIANCE.md). Save an Anthropic API key in Settings → Agents & workspaces → Claude instead")

// Enroll authenticates only an isolated account. Codex owns its refreshable
// login store. Claude is refused here: a subscription token would be a
// Claude.ai credential stored by Lazybox (ErrClaudeTokenImport).
// Call this on the Runner host, before (re)starting the Runner.
func Enroll(ctx context.Context, state, id, provider, binary string, token io.Reader, input io.Reader, output, diagnostic io.Writer, labels ...string) (Profile, error) {
	if !filepath.IsAbs(state) || !profileID.MatchString(id) || (provider != "codex" && provider != "claude") {
		return Profile{}, errors.New("absolute state, safe profile id, and codex/claude provider required")
	}
	dir := filepath.Join(state, "accounts")
	if err := os.MkdirAll(dir, 0700); err != nil {
		return Profile{}, err
	}
	lock, err := os.OpenFile(filepath.Join(dir, "enrollment.lock"), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return Profile{}, err
	}
	defer lock.Close()
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		return Profile{}, errors.New("another account enrollment is running")
	}
	defer syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
	profiles, err := LoadProfiles(state)
	if err != nil {
		return Profile{}, err
	}
	for _, p := range profiles {
		if p.ID == id {
			return Profile{}, errors.New("profile already enrolled; existing credentials were not changed")
		}
	}
	p := Profile{ID: id, Provider: provider, Isolation: "environment", Managed: true}
	if len(labels) > 0 {
		p.Label = labels[0]
	}
	accountDir := filepath.Join(dir, id)
	if err = os.Mkdir(accountDir, 0700); err != nil {
		return Profile{}, fmt.Errorf("new account directory required: %w", err)
	}
	enrolled := false
	defer func() {
		if !enrolled {
			_ = os.RemoveAll(accountDir)
		}
	}()
	if provider == "claude" {
		// Anthropic does not let a third-party app collect or store a Claude.ai
		// credential (CLAUDE_COMPLIANCE.md, R2): no setup-token import.
		return Profile{}, ErrClaudeTokenImport
	} else {
		if token != nil {
			return Profile{}, errors.New("Codex uses its native browser/device login, not token import")
		}
		p.Home = filepath.Join(accountDir, "codex")
		if err = os.Mkdir(p.Home, 0700); err != nil {
			return Profile{}, err
		}
		env, err := Resolve(p, os.Environ())
		if err != nil {
			return Profile{}, err
		}
		if binary == "" {
			binary = "codex"
		}
		command := exec.CommandContext(ctx, binary, "-c", `cli_auth_credentials_store="file"`, "login")
		command.Env = env
		command.Stdin = input
		command.Stdout = output
		command.Stderr = diagnostic
		if err = command.Run(); err != nil {
			return Profile{}, errors.New("isolated Codex login failed")
		}
	}
	directory, err := os.Open(accountDir)
	if err != nil {
		return Profile{}, err
	}
	if err = errors.Join(directory.Sync(), directory.Close()); err != nil {
		return Profile{}, err
	}
	profiles = append(profiles, p)
	if err = writeProfiles(dir, profiles); err != nil {
		// Rename may have committed metadata before a directory-sync failure.
		// Retain its credentials whenever the commit outcome is uncertain.
		if saved, readErr := LoadProfiles(state); readErr == nil {
			for _, registered := range saved {
				if registered.ID == p.ID {
					enrolled = true
				}
			}
		}
		return Profile{}, err
	}
	enrolled = true
	return p, nil
}

func writeProfiles(dir string, profiles []Profile) error {
	data, err := json.MarshalIndent(profiles, "", "  ")
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(dir, ".profiles-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(data)
	if err == nil {
		err = f.Sync()
	}
	err = errors.Join(err, f.Close())
	if err != nil {
		return err
	}
	if err = os.Rename(f.Name(), filepath.Join(dir, "profiles.json")); err != nil {
		return err
	}
	folder, err := os.Open(dir)
	if err != nil {
		return err
	}
	err = errors.Join(folder.Sync(), folder.Close())
	return err
}

// StoreProfile commits an already authenticated isolated profile. Existing
// metadata is never replaced by a login completion or retry.
// RemoveProfile forgets a saved account: its registry entry and its private
// folder (the provider's own sign-in, or an imported token). Other accounts
// and the person's own CLI sign-ins are untouched.
func RemoveProfile(state, id string) error {
	if !filepath.IsAbs(state) || !profileID.MatchString(id) {
		return errors.New("invalid subscription")
	}
	dir := filepath.Join(state, "accounts")
	lock, err := os.OpenFile(filepath.Join(dir, "enrollment.lock"), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		return errors.New("an enrollment is committing; try again")
	}
	defer syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
	profiles, err := LoadProfiles(state)
	if err != nil {
		return err
	}
	kept := profiles[:0]
	for _, p := range profiles {
		if p.ID != id {
			kept = append(kept, p)
		}
	}
	if len(kept) == len(profiles) {
		return os.ErrNotExist
	}
	if err = writeProfiles(dir, kept); err != nil {
		return err
	}
	return os.RemoveAll(filepath.Join(dir, id))
}

func StoreProfile(state string, p Profile) error {
	if !filepath.IsAbs(state) || !profileID.MatchString(p.ID) || (p.Provider != "codex" && p.Provider != "claude") || p.Isolation != "environment" || p.Home != filepath.Join(state, "accounts", p.ID, p.Provider) {
		return errors.New("invalid prepared subscription profile")
	}
	dir := filepath.Join(state, "accounts")
	if err := os.MkdirAll(dir, 0700); err != nil {
		return err
	}
	lock, err := os.OpenFile(filepath.Join(dir, "enrollment.lock"), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		return errors.New("another enrollment is committing")
	}
	defer syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
	profiles, err := LoadProfiles(state)
	if err != nil {
		return err
	}
	for _, old := range profiles {
		if old.ID == p.ID {
			if old.Provider == p.Provider && old.Home == p.Home {
				return nil
			}
			return errors.New("profile identity already exists")
		}
	}
	p.Managed = true
	return writeProfiles(dir, append(profiles, p))
}

// PrepareLogin creates an unused private account directory for native enrollment.
// No account is registered until the provider confirms successful authentication.
func PrepareLogin(state, id, provider, label string) (Profile, error) {
	if !filepath.IsAbs(state) || !profileID.MatchString(id) || (provider != "codex" && provider != "claude") {
		return Profile{}, errors.New("invalid private login identity")
	}
	dir := filepath.Join(state, "accounts", id)
	if err := os.MkdirAll(filepath.Dir(dir), 0700); err != nil {
		return Profile{}, err
	}
	if err := os.Mkdir(dir, 0700); err != nil {
		return Profile{}, err
	}
	p := Profile{ID: id, Label: label, Provider: provider, Isolation: "environment", Managed: true, Home: filepath.Join(dir, provider), NativeLogin: provider == "claude"}
	err := os.Mkdir(p.Home, 0700)
	if err == nil && provider == "claude" {
		err = seedClaudePermissions(p.Home)
	}
	if err != nil {
		_ = os.RemoveAll(dir)
		return Profile{}, err
	}
	return p, nil
}
