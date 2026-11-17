package accounts

import (
	"errors"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

type Profile struct {
	ID    string `json:"id"`
	Label string `json:"label,omitempty"`
	// Derived from the Runner-owned enrollment registry, never configuration.
	Managed  bool   `json:"-"`
	Provider string `json:"provider"`
	// Environment names reference credentials, never literal credentials.
	EnvRefs   map[string]string `json:"envRefs,omitempty"`
	Home      string            `json:"home,omitempty"`
	Isolation string            `json:"isolation"`
	// A Claude setup-token an earlier Lazybox imported; never returned on the
	// wire. No new ones are imported (ErrClaudeTokenImport).
	TokenFile string `json:"tokenFile,omitempty"`
	// A native claude.ai login stored in this managed profile's private directory.
	NativeLogin bool `json:"nativeLogin,omitempty"`
}

// Login is a provider sign-in waiting for the person. A browser sign-in has
// a URL to open and no Code; a device sign-in has a verification URL and the
// one-time Code to enter there.
type Login struct{ ID, URL, Code string }

// How a person signs in to a provider account.
const (
	// BrowserSignIn opens the provider's page in a browser on the Runner's
	// computer; the provider's CLI receives the callback on loopback.
	BrowserSignIn = "browser"
	// DeviceSignIn shows a one-time code to enter on any device.
	DeviceSignIn = "device"
)

type Options struct {
	Instructions     string   `json:"instructions,omitempty"`
	RotationProfiles []string `json:"rotationProfiles,omitempty"`
	// NoRotation keeps the conversation on its selected subscription.
	NoRotation bool           `json:"noRotation,omitempty"`
	Workflow   string         `json:"workflow,omitempty"`
	Model      string         `json:"model,omitempty"`
	Permission string         `json:"permission,omitempty"`
	Gateway    *agent.Gateway `json:"gateway,omitempty"`
}

func (o Options) Validate() error {
	if len(o.Instructions) > 32<<10 {
		return errors.New("session instructions exceed 32 KiB")
	}
	if o.Permission == "yolo" && o.Workflow == "plan" {
		return errors.New("YOLO is incompatible with plan mode")
	}
	if len(o.RotationProfiles) > MaxRotationProfiles {
		return errors.New("at most 16 rotation profiles")
	}
	seen := map[string]bool{}
	for _, id := range o.RotationProfiles {
		if id == "" || seen[id] {
			return errors.New("invalid or duplicate rotation profile")
		}
		seen[id] = true
	}

	if o.Workflow != "" && o.Workflow != "execute" && o.Workflow != "plan" {
		return errors.New("workflow must be execute or plan")
	}
	if o.Permission != "" && o.Permission != "ask" && o.Permission != "read-only" && o.Permission != "yolo" && o.Permission != "inherit" {
		return errors.New("permission must be inherit, ask, read-only or yolo")
	}
	if o.Gateway != nil {
		g := o.Gateway
		u, e := url.Parse(g.URL)
		if e != nil || u.Host == "" || (u.Scheme != "http" && u.Scheme != "https") || u.User != nil || u.Fragment != "" || u.RawQuery != "" || g.ID == "" {
			return errors.New("gateway needs an id and an HTTP(S) URL without credentials, query, or fragment")
		}
	}
	return nil
}

// MaxRotationProfiles bounds a rotation pool (see Options.Validate).
const MaxRotationProfiles = 16

// DefaultRotationPool is the pool a conversation rotates through when nobody
// chose one: every other signed-in (isolated, enrolled) subscription of the
// selected profile's provider, minus the ones excluded from automatic
// switching. IDs are sorted so the cap keeps the same profiles every time.
func DefaultRotationPool(profiles []Profile, selected Profile, excluded []string) []string {
	skip := map[string]bool{selected.ID: true}
	for _, id := range excluded {
		skip[id] = true
	}
	pool := []string{}
	for _, p := range profiles {
		if p.Provider == selected.Provider && p.Isolation == "environment" && !skip[p.ID] {
			skip[p.ID] = true
			pool = append(pool, p.ID)
		}
	}
	sort.Strings(pool)
	if len(pool) > MaxRotationProfiles {
		pool = pool[:MaxRotationProfiles]
	}
	return pool
}

// ACPProvider reports whether provider names an ACP agent ("acp:<id>",
// adapters/acp). An ACP agent runs on its own sign-in: its profile inherits
// the Runner's environment and carries no credentials of its own.
func ACPProvider(provider string) bool {
	return strings.HasPrefix(provider, "acp:") && len(provider) > len("acp:")
}

func Resolve(p Profile, inherited []string) ([]string, error) {
	if p.ID == "" || (p.Provider != "codex" && p.Provider != "claude" && !ACPProvider(p.Provider)) {
		return nil, errors.New("invalid profile identity/provider")
	}
	if ACPProvider(p.Provider) && (p.Isolation == "environment" || p.Home != "" || p.TokenFile != "" || len(p.EnvRefs) > 0) {
		return nil, errors.New("an ACP agent runs on its own sign-in; its profile holds no credentials")
	}
	env := map[string]string{}
	for _, entry := range inherited {
		k, v, ok := strings.Cut(entry, "=")
		if ok {
			env[k] = v
		}
	}

	if p.Isolation == "environment" {
		if err := rejectSharedDirectory(p, env); err != nil {
			return nil, err
		}
		if p.Home == "" && len(p.EnvRefs) == 0 && p.TokenFile == "" {
			return nil, errors.New("isolated profile requires explicit credential context")
		}
		names := []string{"OPENAI_API_KEY", "CODEX_HOME"}
		if p.Provider == "claude" {
			names = []string{"ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_REFRESH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR", "CLAUDE_CODE_HOST_CREDS_FILE", "CLAUDE_CODE_SESSION_ACCESS_TOKEN", "CLAUDE_SESSION_INGRESS_TOKEN_FILE", "ANTHROPIC_PROFILE", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_API_BASE_URL", "CLAUDE_CODE_CUSTOM_OAUTH_URL", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY", "CLAUDE_CODE_USE_ANTHROPIC_AWS", "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE", "CLAUDE_CODE_USE_MANTLE"}
			if p.EnvRefs["ANTHROPIC_API_KEY"] == "" && p.EnvRefs["ANTHROPIC_AUTH_TOKEN"] == "" && p.EnvRefs["CLAUDE_CODE_OAUTH_TOKEN"] == "" && p.TokenFile == "" && !(p.Managed && p.NativeLogin) {
				return nil, errors.New("Claude isolation requires explicit environment credentials; shared keychain isolation is unverified")
			}
		}
		for _, name := range names {
			delete(env, name)
		}
		if p.Provider == "claude" {
			env["DISABLE_LOGIN_COMMAND"] = "1"
			env["DISABLE_LOGOUT_COMMAND"] = "1"
		}
	}
	// Prevent accidental nesting state, while leaving the provider's normal
	// credential resolver intact for the explicitly selected inherited profile.
	delete(env, "CLAUDECODE")
	if p.Home != "" {
		if !filepath.IsAbs(p.Home) {
			return nil, errors.New("profile home must be absolute")
		}
		if p.Provider == "codex" {
			env["CODEX_HOME"] = p.Home
		} else {
			env["CLAUDE_CONFIG_DIR"] = p.Home
		}
	}
	if p.TokenFile != "" {
		// An account an earlier Lazybox imported from a Claude subscription
		// token. No new ones are imported (ErrClaudeTokenImport); an existing
		// one keeps working while Claude sign-in in Lazybox is on (the Runner
		// checks that), and Settings offers to remove it.
		if p.Provider != "claude" || !filepath.IsAbs(p.TokenFile) {
			return nil, errors.New("OAuth token file requires Claude and an absolute path")
		}
		info, err := os.Lstat(p.TokenFile)
		if err != nil {
			return nil, errors.New("OAuth token file unavailable")
		}
		if !info.Mode().IsRegular() || info.Mode().Perm()&0077 != 0 || info.Size() > 16384 {
			return nil, errors.New("OAuth token file must be a private regular file (0600)")
		}
		data, err := os.ReadFile(p.TokenFile)
		if err != nil {
			return nil, errors.New("OAuth token file unreadable")
		}
		token := strings.TrimSpace(string(data))
		if token == "" || strings.ContainsAny(token, "\x00\r\n") {
			return nil, errors.New("invalid OAuth token file")
		}
		env["CLAUDE_CODE_OAUTH_TOKEN"] = token
	}
	for target, ref := range p.EnvRefs {
		if p.Isolation == "environment" && (target == "HOME" || target == "CODEX_HOME" || target == "CLAUDE_CONFIG_DIR") {
			return nil, errors.New("credential references cannot redirect private authentication directories")
		}
		if target == "" || strings.ContainsAny(target, "=\x00") {
			return nil, errors.New("invalid environment target")
		}
		value, ok := os.LookupEnv(ref)
		if !ok || value == "" {
			return nil, fmt.Errorf("credential environment reference %s is unavailable", ref)
		}
		env[target] = value
	}
	keys := make([]string, 0, len(env))
	for k := range env {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	result := make([]string, 0, len(keys))
	for _, k := range keys {
		result = append(result, k+"="+env[k])
	}
	return result, nil
}

type Availability string

const (
	Available  Availability = "available"
	Exhausted  Availability = "exhausted"
	Throttled  Availability = "throttled"
	AuthFailed Availability = "auth_failed"
	Unknown    Availability = "unknown"
)

// Select requires explicit eligibility; errors never rotate profiles implicitly.
func Select(profiles []Profile, eligible []string, states map[string]Availability, provider, current string) (Profile, error) {
	for _, id := range eligible {
		for _, p := range profiles {
			if p.ID == id && p.ID != current && p.Provider == provider && (states[id] == Available || states[id] == "") && p.Isolation == "environment" {
				return p, nil
			}
		}
	}
	return Profile{}, errors.New("no eligible isolated profile has available capacity")
}
