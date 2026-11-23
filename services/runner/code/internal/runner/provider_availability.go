package runner

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"slices"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/codex"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/availability"
)

// claudeProgram is the Claude Code this Runner runs: the configured one, else
// the SDK's bundled build (named "" so the SDK resolves it itself), else one
// on the Runner's PATH, so a Claude Code installed by hand is used when the
// bundled build is missing.
func claudeProgram(cfg Config) string {
	if cfg.ClaudeBinary != "" || availability.BundledClaude(cfg.Worker) != "" {
		return cfg.ClaudeBinary
	}
	if found, err := exec.LookPath("claude"); err == nil {
		return found
	}
	return ""
}

// accountEnv names the variables that sign a provider program in; a model
// listing for the provider report runs without them.
var accountEnv = []string{"CODEX_HOME=", "OPENAI_API_KEY=", "CODEX_API_KEY="}

// programModels lists the models a provider's program offers, without any
// account: Codex's app-server in a scratch CODEX_HOME, removed afterwards, so
// the listing never reads or refreshes a sign-in. Claude Code lists its models
// only in a running session (the worker's command_catalog); the report offers
// the built-in list for it.
func programModels(ctx context.Context, provider, program string) ([]agent.ModelChoice, error) {
	if provider != "codex" {
		return nil, errors.New("only Codex lists its models outside a session")
	}
	home, err := os.MkdirTemp("", "lazybox-codex-models-")
	if err != nil {
		return nil, err
	}
	defer os.RemoveAll(home)
	env := []string{}
	for _, entry := range childEnv() {
		if !slices.ContainsFunc(accountEnv, func(prefix string) bool { return strings.HasPrefix(entry, prefix) }) {
			env = append(env, entry)
		}
	}
	return codex.ListModels(ctx, program, append(env, "CODEX_HOME="+home), home)
}

// ProviderAvailability reports every provider's program (probed once, and
// again when refresh asks) and each saved subscription's state now: what
// Ask Lazybox picks its agent from and the welcome shows.
func (b *Bench) ProviderAvailability(ctx context.Context, refresh bool) (availability.Report, error) {
	if err := b.ReloadProfiles(); err != nil {
		return availability.Report{}, err
	}
	programs, checked := b.Availability.Programs(ctx, refresh)
	subscriptions := availability.Subscriptions(b.Profiles(), b.Accounts.Observation, os.Environ(), time.Now())
	return availability.Build(programs, subscriptions, checked, b.ACPAgents()...), nil
}
