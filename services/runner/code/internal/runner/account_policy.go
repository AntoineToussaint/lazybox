package runner

import (
	"errors"
	"fmt"
	"log"
	"slices"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/preferences"
)

// Account policies (EXTENSIONS.md, "Account policy"). The core keeps several
// signed-in accounts per provider, chosen explicitly per conversation; it
// never moves a conversation to another account by itself. An enabled
// extension whose approved grants include switch:<provider> and whose
// manifest says switchOnLimit is what turns automatic switching on, and
// turning it off stops it at the next limit, running conversations included.

// ErrNoAccountPolicy refuses automatic switching nothing turned on.
var ErrNoAccountPolicy = errors.New("automatic switching is an extension: turn on Subscription rotation in Settings → Extensions")

// AccountPolicy is the account policy that switches conversations of
// provider at a limit, if an enabled extension has one. The first such
// extension, by id, wins.
func (b *Bench) AccountPolicy(provider string) (accounts.Policy, bool) {
	if b.power.Extensions == nil {
		return accounts.Policy{}, false
	}
	for _, e := range b.power.Extensions.Snapshot().Extensions {
		if e.SwitchesFor(provider) {
			return accounts.Policy{Extension: e.ID, Accounts: slices.Clone(e.Manifest.AccountPolicy.Accounts)}, true
		}
	}
	return accounts.Policy{}, false
}

// AutomaticSwitchingProviders are the providers an account policy switches.
func (b *Bench) AutomaticSwitchingProviders() []string {
	out := []string{}
	for _, provider := range []string{"claude", "codex"} {
		if _, ok := b.AccountPolicy(provider); ok {
			out = append(out, provider)
		}
	}
	return out
}

// rotationPool resolves which subscriptions a new conversation may switch to
// by itself: none without an account policy for its provider. With one, an
// explicit opt-out wins, then an explicit pool, then the configured default
// (an explicit empty list there disables it), then the policy's own accounts.
func rotationPool(options accounts.Options, configured []string, profiles []accounts.Profile, selected accounts.Profile, excluded []string, policy accounts.Policy, switching bool) []string {
	switch {
	case !switching, options.NoRotation:
		return nil
	case options.RotationProfiles != nil:
		return options.RotationProfiles
	case configured != nil:
		return configured
	}
	return policy.Pool(profiles, selected, excluded)
}

// One-time notices (RunnerSettings.notices).
const (
	// NoticeSwitchingMoved: automatic switching was on and is now the
	// Subscription rotation extension, which the migration turned on.
	NoticeSwitchingMoved = "automatic-switching-moved"
	// NoticeClaudeSignInKept: Lazybox now follows Anthropic's terms for Claude
	// sign-in; the Claude accounts already saved keep working.
	NoticeClaudeSignInKept = "claude-sign-in-kept"
)

// RotationExtension is the bundled extension whose account policy is the
// automatic switching the core used to do.
const RotationExtension = "subscription-rotation"

// accountsMigration is the preferences migration that moved automatic
// switching out of the core and gated Claude sign-in (CLAUDE_COMPLIANCE.md).
const accountsMigration = 1

// migrateAccounts runs once per Runner state: compliant defaults for new
// state, and an existing setup keeps working. With Claude subscriptions
// already saved in Lazybox (signed in or imported from a token), Claude
// sign-in in Lazybox is carried over on. Where automatic switching was in
// effect (a signed-in subscription not excluded, beside another account of
// its provider, or a configured pool), the bundled Subscription rotation
// extension is installed and approved, so it goes on switching with the same
// settings (exclusions and configured pools stay where they were). Each change
// is said once, in a notice.
func (b *Bench) migrateAccounts() error {
	data := b.Preferences.Snapshot()
	if data.Migration >= accountsMigration {
		return nil
	}
	var notices []string
	add := func(id string) {
		if !slices.Contains(notices, id) {
			notices = append(notices, id)
		}
	}
	if len(b.Config.Defaults.RotationProfiles) > 0 {
		add(NoticeSwitchingMoved)
	}
	profiles := b.Profiles()
	for _, p := range profiles {
		// It was in some conversation's default pool: a signed-in account
		// not excluded, beside another account of its provider.
		another := slices.ContainsFunc(profiles, func(q accounts.Profile) bool { return q.ID != p.ID && q.Provider == p.Provider })
		if p.Isolation == "environment" && another && !slices.Contains(data.RotationExcluded, p.ID) {
			add(NoticeSwitchingMoved)
		}
		if p.Provider == "claude" && p.Managed && (p.NativeLogin || p.TokenFile != "") {
			add(NoticeClaudeSignInKept)
		}
	}
	if slices.Contains(notices, NoticeSwitchingMoved) {
		// A Runner always starts: a folder that will not take the extension
		// is said in the log, and the notice still points at Settings.
		if err := b.keepRotation(); err != nil {
			log.Printf("account migration: %v", err)
		}
	}
	return b.Preferences.Migrate(accountsMigration, func(d *preferences.Data) {
		for _, id := range notices {
			if !slices.Contains(d.Notices, id) {
				d.Notices = append(d.Notices, id)
			}
		}
		if slices.Contains(notices, NoticeClaudeSignInKept) {
			d.ClaudeSubscriptionSignIn, d.ClaudeSignInCarriedOver = true, true
		}
	})
}

// keepRotation installs the bundled Subscription rotation extension and
// approves exactly what it asks for, as the person would in Settings →
// Extensions, so automatic switching that was on stays on. A copy already in
// the folder is approved as it is, unless the person turned it off.
func (b *Bench) keepRotation() error {
	ext := b.power.Extensions
	if ext == nil {
		return nil
	}
	snap := ext.Snapshot()
	if _, ok := snap.Find(RotationExtension); !ok {
		var err error
		if snap, err = ext.InstallExample(RotationExtension); err != nil {
			return fmt.Errorf("keeping automatic switching: %w", err)
		}
	}
	info, ok := snap.Find(RotationExtension)
	if !ok || info.State == power.ExtensionEnabled || info.State == power.ExtensionDisabled {
		return nil
	}
	if _, err := ext.Approve(info.ID, info.Manifest.Grants, info.Manifest.Hosts); err != nil {
		return fmt.Errorf("keeping automatic switching: %w", err)
	}
	return nil
}
