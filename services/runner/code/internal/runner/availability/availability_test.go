package availability

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/acp"
)

func program(t *testing.T, path string) string {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("#!/bin/sh\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	return path
}

// fakeProbe finds programs only in bin, and answers --version from versions.
func fakeProbe(bin string, versions map[string]string) Probe {
	return Probe{
		LookPath: func(name string) (string, error) {
			if path := filepath.Join(bin, name); executable(path) {
				return path, nil
			}
			return "", errors.New("not found")
		},
		Version: func(_ context.Context, path string) (string, error) {
			if v, ok := versions[filepath.Base(path)]; ok {
				return v, nil
			}
			return "", errors.New("exit status 1")
		},
	}
}

func bundled(t *testing.T, root string) string {
	arch := map[string]string{"amd64": "x64", "arm64": "arm64"}[runtime.GOARCH]
	return program(t, filepath.Join(root, "node_modules", "@anthropic-ai", "claude-agent-sdk-"+runtime.GOOS+"-"+arch, "claude"))
}

func TestProgramsFindTheBundledClaudeAndCodexOnThePath(t *testing.T) {
	root, bin := t.TempDir(), t.TempDir()
	// The worker sits two folders below the node_modules Node resolves the SDK from.
	worker := program(t, filepath.Join(root, "workers", "claude-sdk", "dist", "main.js"))
	claude := bundled(t, root)
	program(t, filepath.Join(bin, "codex"))
	program(t, filepath.Join(bin, "node"))
	probe := fakeProbe(bin, map[string]string{"claude": "2.1.311 (Claude Code)", "codex": "codex-cli 0.98.0"})
	probe.Worker = worker
	got := probe.Programs(context.Background())
	if c := got["claude"]; !c.Installed || c.Path != claude || c.Source != "bundled" || c.Version != "2.1.311" || c.Error != "" {
		t.Fatalf("claude: %+v", c)
	}
	if c := got["codex"]; !c.Installed || c.Path != filepath.Join(bin, "codex") || c.Source != "path" || c.Version != "0.98.0" || c.Error != "" {
		t.Fatalf("codex: %+v", c)
	}
}

func TestProgramsSayWhatIsMissingOrBroken(t *testing.T) {
	root, bin := t.TempDir(), t.TempDir()
	worker := filepath.Join(root, "dist", "main.js")
	probe := fakeProbe(bin, map[string]string{"claude": "2.1.311 (Claude Code)"})
	probe.Worker = worker
	got := probe.Programs(context.Background())
	// Nothing anywhere: not installed, with how to install it.
	if c := got["claude"]; c.Installed || !strings.Contains(c.Error, "not installed") {
		t.Fatalf("claude missing: %+v", c)
	}
	if c := got["codex"]; c.Installed || !strings.Contains(c.Error, "Codex is not installed") {
		t.Fatalf("codex missing: %+v", c)
	}
	report := Build(got, nil, time.Now())
	if report.Providers[0].State != "not_installed" || report.Providers[0].InstallCommand != InstallCommands["claude"] || report.Providers[1].InstallCommand != InstallCommands["codex"] {
		t.Fatalf("report: %+v", report.Providers)
	}

	// A Claude Code on the PATH is used when the bundled build is missing; the
	// worker it runs through is missing too, so it cannot run.
	program(t, filepath.Join(bin, "claude"))
	// One installed after the Runner started is not the one its adapter runs.
	if c := probe.Programs(context.Background())["claude"]; !c.Installed || c.Source != "path" || !strings.Contains(c.Error, "Restart the Runner") {
		t.Fatalf("claude installed later: %+v", c)
	}
	probe.ClaudeRunning = filepath.Join(bin, "claude")
	if c := probe.Programs(context.Background())["claude"]; !c.Installed || c.Source != "path" || !strings.Contains(c.Error, "worker") {
		t.Fatalf("claude without worker: %+v", c)
	}
	program(t, worker)
	if c := probe.Programs(context.Background())["claude"]; !strings.Contains(c.Error, "Node") {
		t.Fatalf("claude without node: %+v", c)
	}
	program(t, filepath.Join(bin, "node"))
	if c := probe.Programs(context.Background())["claude"]; c.Error != "" {
		t.Fatalf("claude: %+v", c)
	}

	// A configured program wins, and one that does not run says so.
	probe.CodexBinary = program(t, filepath.Join(root, "custom", "codex-nightly"))
	c := probe.Programs(context.Background())["codex"]
	if !c.Installed || c.Source != "configured" || !strings.Contains(c.Error, "does not run") {
		t.Fatalf("broken codex: %+v", c)
	}
	if state := Build(map[string]Program{"codex": c}, map[string][]Subscription{"codex": {{ProfileID: "codex-default", State: "available"}}}, time.Now()).Providers[1].State; state != "unusable" {
		t.Fatalf("broken codex state %s", state)
	}
	probe.ClaudeBinary = filepath.Join(root, "nowhere", "claude")
	if c := probe.Programs(context.Background())["claude"]; c.Installed || c.Source != "configured" || !strings.Contains(c.Error, "nothing runs there") {
		t.Fatalf("configured claude missing: %+v", c)
	}
}

// The report offers the models Codex lists, cached with its program, and
// the built-in ones when it lists none, for Claude, or when listing fails.
func TestReportOffersListedModelsElseBuiltIn(t *testing.T) {
	bin := t.TempDir()
	program(t, filepath.Join(bin, "codex"))
	probe := fakeProbe(bin, map[string]string{"codex": "codex-cli 0.169.0"})
	listed := []agent.ModelChoice{{Value: "gpt-6.1-sol", DisplayName: "GPT-6.1-Sol"}, {Value: "gpt-6-luna", DisplayName: "GPT-6-Luna"}}
	asked := []string{}
	probe.Models = func(_ context.Context, provider, path string) ([]agent.ModelChoice, error) {
		asked = append(asked, provider+" "+filepath.Base(path))
		return listed, nil
	}
	cache := &Cache{Probe: probe}
	programs, at := cache.Programs(context.Background(), false)
	cache.Programs(context.Background(), false)
	if len(asked) != 1 || asked[0] != "codex codex" {
		t.Fatalf("listed %v: once, Codex only", asked)
	}
	report := Build(programs, nil, at)
	if got := report.Providers[1].Models; len(got) != 2 || got[0].Value != "gpt-6.1-sol" {
		t.Fatalf("codex models: %+v", got)
	}
	// Claude lists its models only in a session: the built-in ones.
	if got := report.Providers[0].Models; len(got) != len(BuiltinModels["claude"]) || got[0].Value != "claude-opus-5-5" {
		t.Fatalf("claude models: %+v", got)
	}
	names := []string{}
	for _, m := range BuiltinModels["claude"] {
		names = append(names, m.Value)
	}
	if strings.Join(names, " ") != "claude-opus-5-5 claude-sonnet-5-5 claude-haiku-5-5 claude-fable-5-2" {
		t.Fatalf("claude built-in models: %v", names)
	}
	// A failed listing is not a broken program: the built-in list stands in.
	probe.Models = func(context.Context, string, string) ([]agent.ModelChoice, error) {
		return nil, errors.New("unknown method")
	}
	codex := probe.Programs(context.Background())["codex"]
	if codex.Error != "" || codex.Models != nil {
		t.Fatalf("codex after a failed listing: %+v", codex)
	}
	if got := Build(map[string]Program{"codex": codex}, nil, at).Providers[1].Models; len(got) != len(BuiltinModels["codex"]) {
		t.Fatalf("codex fallback models: %+v", got)
	}
	// The report's list is its own: changing it never changes the built-in one.
	report.Providers[0].Models[0].Value = "changed"
	if BuiltinModels["claude"][0].Value != "claude-opus-5-5" {
		t.Fatal("a report shares the built-in list")
	}
}

func TestCacheProbesOnceUntilRefreshed(t *testing.T) {
	bin := t.TempDir()
	program(t, filepath.Join(bin, "codex"))
	calls := 0
	probe := fakeProbe(bin, nil)
	probe.Version = func(context.Context, string) (string, error) { calls++; return "codex-cli 1.0.0", nil }
	cache := &Cache{Probe: probe}
	first, at := cache.Programs(context.Background(), false)
	again, sameAt := cache.Programs(context.Background(), false)
	if calls != 1 || at != sameAt || first["codex"].Version != again["codex"].Version {
		t.Fatalf("cached: %d probes", calls)
	}
	if _, later := cache.Programs(context.Background(), true); calls != 2 || !later.After(at) && !later.Equal(at) {
		t.Fatalf("refresh: %d probes", calls)
	}
}

func TestSubscriptionsSayWhichCanTakeWorkBestFirst(t *testing.T) {
	home, state := t.TempDir(), t.TempDir()
	now := time.Unix(1_800_000_000, 0)
	env := []string{"HOME=" + home}
	// The CLI's own Claude sign-in exists; the Codex one does not.
	if err := os.WriteFile(filepath.Join(home, ".claude.json"), []byte(`{"oauthAccount":{"emailAddress":"me@example.invalid"}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	saved := func(id, provider string) accounts.Profile {
		p := accounts.Profile{ID: id, Provider: provider, Label: id, Managed: true, Isolation: "environment", Home: filepath.Join(state, "accounts", id, provider)}
		if err := os.MkdirAll(p.Home, 0o700); err != nil {
			t.Fatal(err)
		}
		if provider == "codex" {
			if err := os.WriteFile(filepath.Join(p.Home, "auth.json"), []byte(`{}`), 0o600); err != nil {
				t.Fatal(err)
			}
		}
		return p
	}
	profiles := []accounts.Profile{
		{ID: "claude-default", Provider: "claude", Isolation: "inherited"},
		{ID: "codex-default", Provider: "codex", Isolation: "inherited"},
		saved("codex-limited", "codex"),
		saved("codex-reset", "codex"),
		saved("codex-later", "codex"),
		saved("codex-expired", "codex"),
		saved("codex-billing", "codex"),
	}
	observations := map[string]agent.AccountObservation{
		"codex-limited": {State: "exhausted", ResetAt: now.Unix() + 3600},
		// Its reset passed: the next use tells, and its sign-in is there.
		"codex-reset": {State: "exhausted", ResetAt: now.Unix() - 60},
		// A window at 100% resets later than the account's own reset.
		"codex-later":   {State: "throttled", ResetAt: now.Unix() + 60, Windows: []agent.UsageWindow{{Name: "weekly", UsedPercent: 100, ResetAt: now.Unix() + 7200}}},
		"codex-expired": {State: "auth_required"},
		"codex-billing": {State: "billing_error"},
	}
	observe := func(id string) agent.AccountObservation {
		if o, ok := observations[id]; ok {
			return o
		}
		return agent.AccountObservation{State: "unknown"}
	}
	got := Subscriptions(profiles, observe, env, now)
	if c := got["claude"]; len(c) != 1 || c[0].State != "available" || c[0].Detail != "Signed in as me@example.invalid" {
		t.Fatalf("claude: %+v", c)
	}
	order := func(list []Subscription) string {
		var ids []string
		for _, s := range list {
			ids = append(ids, s.ProfileID+":"+s.State)
		}
		return strings.Join(ids, " ")
	}
	want := "codex-reset:available codex-limited:limited codex-later:limited codex-billing:billing codex-expired:sign_in codex-default:sign_in"
	if order(got["codex"]) != want {
		t.Fatalf("codex order\n got %s\nwant %s", order(got["codex"]), want)
	}
	if later := got["codex"][2]; later.ResetAt != now.Unix()+7200 {
		t.Fatalf("a limit lasts until its full windows reset: %+v", later)
	}
	report := Build(map[string]Program{"claude": {Installed: true}, "codex": {Installed: true}}, got, now)
	if report.Providers[0].State != "ready" || report.Providers[1].State != "ready" {
		t.Fatalf("states: %+v", report.Providers)
	}

	// Every Codex subscription at a limit: the provider is limited; none signed in: sign_in.
	delete(observations, "codex-reset")
	observations["codex-reset"] = agent.AccountObservation{State: "throttled", ResetAt: now.Unix() + 30}
	report = Build(map[string]Program{"claude": {Installed: true}, "codex": {Installed: true}}, Subscriptions(profiles[1:], observe, env, now), now)
	if report.Providers[1].State != "limited" || report.Providers[1].Subscriptions[0].ProfileID != "codex-reset" {
		t.Fatalf("limited: %+v", report.Providers[1])
	}
	if report.Providers[0].State != "no_subscription" {
		t.Fatalf("claude without profiles: %+v", report.Providers[0])
	}
	report = Build(map[string]Program{"codex": {Installed: true}}, Subscriptions(profiles[1:2], observe, env, now), now)
	if report.Providers[1].State != "sign_in" {
		t.Fatalf("signed out: %+v", report.Providers[1])
	}
}

// A credential kept where it cannot be checked is not a reason to skip the
// subscription: it is usable, below one known to be available.
func TestUnverifiedSubscriptionsAreUsable(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".codex"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, ".codex", "config.toml"), []byte("cli_auth_credentials_store = \"keyring\"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	got := Subscriptions([]accounts.Profile{{ID: "codex-default", Provider: "codex", Isolation: "inherited"}}, func(string) agent.AccountObservation { return agent.AccountObservation{State: "unknown"} }, []string{"HOME=" + home}, time.Now())
	if s := got["codex"][0]; s.State != "unverified" || !s.Usable() {
		t.Fatalf("keyring: %+v", s)
	}
	if state := Build(map[string]Program{"codex": {Installed: true}}, got, time.Now()).Providers[1].State; state != "ready" {
		t.Fatalf("state %s", state)
	}
}

// A built-in ACP agent's launch flags are checked against the installed
// program's --help: a flag or subcommand it does not name is said, with the
// fix; a custom command (a launcher such as npx) is not checked; a help that
// cannot be read checks nothing.
func TestACPAgentLaunchFlagsAreCheckedAgainstTheirHelp(t *testing.T) {
	bin := t.TempDir()
	for _, name := range []string{"gemini", "goose", "qwen", "opencode"} {
		program(t, filepath.Join(bin, name))
	}
	helps := map[string]string{
		"gemini":   "Usage: gemini [options]\n  --experimental-acp  Starts the agent in ACP mode\n  -m, --model",
		"goose":    "Commands:\n  session  Start a session\n  acp      Run goose as an ACP agent\n",
		"qwen":     "Options:\n  [--acp]  Agent Client Protocol mode",
		"opencode": "",
	}
	probe := fakeProbe(bin, map[string]string{})
	probe.Help = func(_ context.Context, path string) (string, error) {
		help := helps[filepath.Base(path)]
		if help == "" {
			return "", errors.New("printed no help")
		}
		return help, nil
	}
	var agents []acp.Agent
	for _, a := range acp.Builtins {
		if _, ok := helps[a.ID]; ok && a.ID != "qwen" {
			agents = append(agents, a)
		}
	}
	// The person changed how Qwen Code starts: their command is not checked.
	custom := acp.Agent{ID: "qwen", Title: "Qwen Code", Command: "qwen", Args: []string{"--not-in-help"}, Custom: true}
	probe.Agents = func() []acp.Agent { return append(append([]acp.Agent(nil), agents...), custom) }
	got := probe.Programs(context.Background())
	if p := got[acp.Provider("gemini")].LaunchProblem; p != "Gemini CLI's help does not mention `--acp`, which Lazybox starts it with: this version may not run in ACP mode. Update it (`npm install -g @google/gemini-cli`), or edit how Lazybox starts it." {
		t.Fatalf("gemini: %q", p)
	}
	for _, id := range []string{"goose", "opencode", "qwen"} {
		if p := got[acp.Provider(id)].LaunchProblem; p != "" {
			t.Fatalf("%s: %q", id, p)
		}
	}
	// A built-in Qwen whose help names --acp in brackets passes.
	for _, a := range acp.Builtins {
		if a.ID == "qwen" && a.LaunchProblem(helps["qwen"]) != "" {
			t.Fatal("[--acp] is named")
		}
	}
	report := Build(got, nil, time.Now(), probe.Agents()...)
	found := false
	for _, p := range report.Providers {
		if p.ACP != nil && p.ACP.ID == "gemini" {
			found = strings.Contains(p.ACP.LaunchProblem, "`--acp`")
		}
	}
	if !found {
		t.Fatal("the report does not carry the problem")
	}
}
