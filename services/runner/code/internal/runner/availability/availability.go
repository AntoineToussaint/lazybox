// Package availability reports which agent providers a Runner can run and
// on which saved subscriptions: each provider's program (found, where, which
// version) and each subscription's state. Ask Lazybox picks its agent from
// the report and the welcome shows it, so a person sees what is missing
// before anything fails.
//
// Programs are probed (one `--version` each) and cached until a refresh;
// subscription states are cheap and computed on every report.
package availability

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/acp"
)

// Providers the Runner runs, in the order every surface lists them.
var Providers = []string{"claude", "codex"}

// Titles are the programs' names as people know them.
var Titles = map[string]string{"claude": "Claude Code", "codex": "Codex"}

// InstallCommands install a missing provider. Claude Code normally ships with
// Lazybox (the SDK's own build); this installs one the Runner finds on its
// PATH when that build is missing.
var InstallCommands = map[string]string{
	"claude": "curl -fsSL https://claude.ai/install.sh | bash",
	"codex":  "npm install -g @openai/codex",
}

// Program is a provider's executable as the Runner would run it.
type Program struct {
	Installed bool
	Path      string
	Version   string
	// Source: "configured" (the Runner configuration names it), "bundled"
	// (the build the Claude SDK ships) or "path" (found on the Runner's PATH).
	Source string
	// Error says, in plain words, why the provider cannot run; empty when it can.
	Error string
	// Models are the models the program itself lists (Codex's model/list),
	// nil when it lists none or was not asked.
	Models []agent.ModelChoice
	// LaunchProblem, for a built-in ACP agent, says its help does not name
	// the flags Lazybox starts it with (acp.Agent.LaunchProblem); a hint for
	// Settings, not a reason it cannot run.
	LaunchProblem string
}

// Subscription is one saved profile and whether it can take work now.
type Subscription struct {
	ProfileID string
	Label     string
	// State: available, unverified, limited, sign_in or billing.
	State   string
	ResetAt int64
	Detail  string
	Managed bool
	// Isolation is the profile's: "inherited" for the CLI's own sign-in.
	Isolation string
}

// Usable reports whether a subscription can be picked without asking.
func (s Subscription) Usable() bool { return s.State == "available" || s.State == "unverified" }

// ACPAgent is what a report says of an ACP agent beyond its program: how
// it is started (never its environment's values) and how it is shown.
type ACPAgent struct {
	ID       string
	Icon     string
	Command  string
	Args     []string
	Env      []string
	Homepage string
	// SharesCredentialsWith: the agent whose credentials it uses when it has
	// none; InheritedEnv the names it takes from it now (never the values).
	SharesCredentialsWith string
	InheritedEnv          []string
	// SignIn and CredentialsNote: how a built-in agent signs in.
	SignIn          string
	CredentialsNote string
	// Custom: the person added it, or changed a built-in one's command.
	Custom bool
	// LaunchProblem: the program's help does not name how Lazybox starts it.
	LaunchProblem string
}

// Provider is one provider's program and subscriptions, with their summary.
type Provider struct {
	Provider string
	Title    string
	// ACP is set for an ACP agent (adapters/acp), nil for Claude and Codex.
	ACP *ACPAgent
	// State: ready, limited, sign_in, billing, no_subscription, not_installed
	// or unusable.
	State          string
	Program        Program
	InstallCommand string
	Subscriptions  []Subscription
	// Models a conversation of this provider can switch to: what the program
	// lists, else BuiltinModels. A running agent's own list (its
	// command_catalog) is more precise; this is the fallback when it has none.
	Models []agent.ModelChoice
}

// BuiltinModels are each provider's current models, for when neither the
// program nor a running agent lists any: an agent started by an older
// Lazybox, or a Codex without model/list. Ids the provider takes; names as
// people say them.
var BuiltinModels = map[string][]agent.ModelChoice{
	"claude": {
		{Value: "claude-opus-5-5", DisplayName: "Opus 5.5", Description: "For complex work and everyday tasks"},
		{Value: "claude-sonnet-5-5", DisplayName: "Sonnet 5.5", Description: "Most efficient for simpler tasks"},
		{Value: "claude-haiku-5-5", DisplayName: "Haiku 5.5", Description: "Fastest for quick answers"},
		{Value: "claude-fable-5-2", DisplayName: "Fable 5.2", Description: "For your toughest challenges"},
	},
	"codex": {
		{Value: "gpt-6.2-sol", DisplayName: "GPT-6.2 Sol", Description: "For everyday coding and professional work"},
		{Value: "gpt-6.2-astra", DisplayName: "GPT-6.2 Astra", Description: "For demanding reasoning and coding"},
		{Value: "gpt-6.1-sol", DisplayName: "GPT-6.1-Sol", Description: "Workhorse model for coding and everyday work"},
		{Value: "gpt-6-astra", DisplayName: "GPT-6-Astra", Description: "Frontier intelligence for the most demanding work"},
		{Value: "gpt-6.1-terra", DisplayName: "GPT-6.1 Terra", Description: "Balanced model for straightforward work"},
		{Value: "gpt-6-luna", DisplayName: "GPT-6-Luna", Description: "Fast and affordable model for easier tasks"},
	},
}

// models is what a report offers for provider: the program's list, else the built-in one.
func models(provider string, program Program) []agent.ModelChoice {
	if len(program.Models) > 0 {
		return append([]agent.ModelChoice(nil), program.Models...)
	}
	return append([]agent.ModelChoice(nil), BuiltinModels[provider]...)
}

// Report is every provider, in Providers order.
type Report struct {
	Providers []Provider
	CheckedAt time.Time
}

// Probe finds the provider programs. Its fields mirror the Runner
// configuration; LookPath and Version are injected so a test can fake them.
type Probe struct {
	// Worker and Node run Claude: the SDK worker script and the Node that runs it.
	Worker, Node string
	// ClaudeBinary and CodexBinary are the configured programs; empty finds them.
	ClaudeBinary, CodexBinary string
	// ClaudeRunning is the Claude Code the Runner's adapter was started with
	// ("" for the bundled build). A Claude Code found on the PATH only after
	// the Runner started is reported as needing a restart.
	ClaudeRunning string
	// Agents lists the ACP agents the Runner can run (the built-ins and the
	// person's own); nil reports none.
	Agents func() []acp.Agent
	// AgentProgram finds an ACP agent's executable ("" when missing); nil
	// looks it up with LookPath.
	AgentProgram func(acp.Agent) string
	// LookPath finds a program on the Runner's PATH (exec.LookPath).
	LookPath func(string) (string, error)
	// Version runs `program --version` and returns its first line.
	Version func(ctx context.Context, program string) (string, error)
	// Help runs `program --help` and returns what it printed; nil checks no
	// ACP agent's launch flags.
	Help func(ctx context.Context, program string) (string, error)
	// Models lists the models a provider's program offers, without any
	// account (Codex: an app-server's model/list); nil asks nothing, and the
	// report offers BuiltinModels.
	Models func(ctx context.Context, provider, program string) ([]agent.ModelChoice, error)
}

// DefaultProbe probes with the Runner's own PATH and runs each program.
// running is the Claude Code the Runner's adapter runs.
func DefaultProbe(worker, node, claude, codex, running string) Probe {
	return Probe{Worker: worker, Node: node, ClaudeBinary: claude, CodexBinary: codex, ClaudeRunning: running, LookPath: exec.LookPath, Version: RunVersion, Help: RunHelp}
}

// RunHelp runs `program --help`, bounded, with no input, and returns what it
// printed on either stream (at most 256 KiB). Some programs print their help
// on stderr or exit non-zero after printing it: whatever was printed counts.
func RunHelp(ctx context.Context, program string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, program, "--help")
	cmd.Env = append(os.Environ(), "NO_COLOR=1", "CI=1")
	out, err := cmd.CombinedOutput()
	if len(out) > 256<<10 {
		out = out[:256<<10]
	}
	if len(strings.TrimSpace(string(out))) > 0 {
		return string(out), nil
	}
	if err == nil {
		err = errors.New("printed no help")
	}
	return "", err
}

// RunVersion runs `program --version`, bounded, and returns its first line.
func RunVersion(ctx context.Context, program string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, program, "--version").Output()
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(strings.SplitN(string(out), "\n", 2)[0]), nil
}

// BundledClaude is the Claude Code the SDK ships beside worker, found the way
// Node resolves the SDK's platform package from it; "" when it is missing.
func BundledClaude(worker string) string {
	if worker == "" {
		return ""
	}
	arch := map[string]string{"amd64": "x64", "arm64": "arm64"}[runtime.GOARCH]
	if arch == "" {
		return ""
	}
	pkg := filepath.Join("node_modules", "@anthropic-ai", "claude-agent-sdk-"+runtime.GOOS+"-"+arch, "claude")
	for dir := filepath.Dir(worker); ; dir = filepath.Dir(dir) {
		if candidate := filepath.Join(dir, pkg); executable(candidate) {
			return candidate
		}
		if parent := filepath.Dir(dir); parent == dir {
			return ""
		}
	}
}

func executable(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0
}

// ClaudeProgram is the Claude Code a Runner with this configuration runs: the
// configured one, else the SDK's bundled build, else one on the PATH. Empty
// when there is none.
func (p Probe) ClaudeProgram() (path, source string) {
	if p.ClaudeBinary != "" {
		return p.ClaudeBinary, "configured"
	}
	if bundled := BundledClaude(p.Worker); bundled != "" {
		return bundled, "bundled"
	}
	if p.LookPath != nil {
		if found, err := p.LookPath("claude"); err == nil {
			return found, "path"
		}
	}
	return "", ""
}

// Programs probes every provider's program, ACP agents' too, concurrently.
func (p Probe) Programs(ctx context.Context) map[string]Program {
	result := map[string]Program{}
	var mu sync.Mutex
	var wg sync.WaitGroup
	for _, provider := range Providers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			program := p.program(ctx, provider)
			mu.Lock()
			result[provider] = program
			mu.Unlock()
		}()
	}
	if p.Agents != nil {
		for _, a := range p.Agents() {
			wg.Add(1)
			go func() {
				defer wg.Done()
				program := p.agentProgram(ctx, a)
				mu.Lock()
				result[acp.Provider(a.ID)] = program
				mu.Unlock()
			}()
		}
	}
	wg.Wait()
	return result
}

// agentProgram finds an ACP agent's program and, for a built-in one, asks
// its version. A custom command may be a launcher (npx, uvx) whose own
// version would say nothing about the agent, so it is not asked.
func (p Probe) agentProgram(ctx context.Context, a acp.Agent) Program {
	path := a.Program(p.LookPath)
	if p.AgentProgram != nil {
		path = p.AgentProgram(a)
	}
	if path == "" {
		if filepath.IsAbs(a.Command) {
			return Program{Path: a.Command, Source: "configured", Error: a.Title + " is configured at " + a.Command + ", but nothing runs there."}
		}
		return Program{Error: a.Title + " is not installed on the Runner's computer (no " + a.Command + " on its PATH)."}
	}
	program := Program{Installed: true, Path: path, Source: "path"}
	if a.Custom {
		program.Source = "configured"
		return program
	}
	if p.Version != nil {
		if line, err := p.Version(ctx, path); err == nil {
			program.Version = version(line)
		}
	}
	// The built-in launch flags follow the ACP registry, not the installed
	// version: check them against what this one says of itself. A help that
	// cannot be read checks nothing.
	if p.Help != nil {
		if help, err := p.Help(ctx, path); err == nil {
			program.LaunchProblem = a.LaunchProblem(help)
		}
	}
	return program
}

func (p Probe) program(ctx context.Context, provider string) Program {
	var path, source string
	if provider == "claude" {
		path, source = p.ClaudeProgram()
	} else if p.CodexBinary != "" {
		path, source = p.CodexBinary, "configured"
	} else if p.LookPath != nil {
		if found, err := p.LookPath("codex"); err == nil {
			path, source = found, "path"
		}
	}
	title := Titles[provider]
	if path == "" {
		return Program{Error: title + " is not installed on the Runner's computer."}
	}
	program := Program{Installed: true, Path: path, Source: source}
	if !executable(path) {
		program.Installed = false
		program.Error = title + " is configured at " + path + ", but nothing runs there."
		return program
	}
	line, err := p.Version(ctx, path)
	if err != nil {
		program.Error = title + " at " + path + " does not run: " + err.Error()
		return program
	}
	program.Version = version(line)
	if p.Models != nil && provider == "codex" {
		// A listing that fails is not the program failing: the built-in list stands in.
		if listed, err := p.Models(ctx, provider, path); err == nil {
			program.Models = listed
		}
	}
	if provider == "claude" {
		// Lazybox runs Claude through its SDK worker, which Node runs.
		if source == "path" && path != p.ClaudeRunning {
			program.Error = "Claude Code at " + path + " was installed after the Runner started. Restart the Runner (Settings → Runner) to use it."
		} else if p.Worker == "" || !regular(p.Worker) {
			program.Error = "The Claude worker that Lazybox runs Claude Code with is missing. Reinstall Lazybox."
		} else if node := p.Node; node == "" && p.LookPath != nil {
			if _, err := p.LookPath("node"); err != nil {
				program.Error = "Node, which runs the Claude worker, is not on the Runner's PATH."
			}
		} else if node != "" && !executable(node) {
			program.Error = "The Node that runs the Claude worker is missing at " + node + "."
		}
	}
	return program
}

func regular(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

var versionPattern = regexp.MustCompile(`\d+\.\d+(?:\.\d+)?(?:[-+][0-9A-Za-z.]+)?`)

// version is the version number in a program's --version line ("2.1.311
// (Claude Code)" → "2.1.311", "codex-cli 0.98.0" → "0.98.0"), else the line.
func version(line string) string {
	if v := versionPattern.FindString(line); v != "" {
		return v
	}
	return line
}

// Subscriptions is each provider's saved profiles with their state, best
// first: what a provider's limits and sign-ins say now. observe returns the
// Runner's last account observation of a profile; env is the Runner's
// environment, against which a profile's credentials resolve.
func Subscriptions(profiles []accounts.Profile, observe func(string) agent.AccountObservation, env []string, now time.Time) map[string][]Subscription {
	result := map[string][]Subscription{}
	for _, p := range profiles {
		result[p.Provider] = append(result[p.Provider], subscription(p, observe(p.ID), env, now))
	}
	for provider, list := range result {
		sort.SliceStable(list, func(i, j int) bool { return better(list[i], list[j]) })
		result[provider] = list
	}
	return result
}

func subscription(p accounts.Profile, seen agent.AccountObservation, env []string, now time.Time) Subscription {
	s := Subscription{ProfileID: p.ID, Label: p.Label, Managed: p.Managed, Isolation: p.Isolation, ResetAt: seen.ResetAt}
	// A limit lasts until every window at 100% has reset.
	for _, w := range seen.Windows {
		if w.UsedPercent >= 100 && w.ResetAt > s.ResetAt {
			s.ResetAt = w.ResetAt
		}
	}
	switch seen.State {
	case "auth_required":
		s.State, s.Detail = "sign_in", "Needs you to sign in again"
		return s
	case "billing_error":
		s.State, s.Detail = "billing", "Has a billing problem"
		return s
	case "exhausted", "throttled":
		if s.ResetAt == 0 || s.ResetAt > now.Unix() {
			s.State, s.Detail = "limited", map[string]string{"exhausted": "Reached its limit", "throttled": "Rate limited"}[seen.State]
			return s
		}
		// The reset passed: the next use tells.
		s.ResetAt = 0
	}
	signIn, reason := accounts.CheckSignIn(p, env)
	switch {
	case signIn == accounts.SignedOut:
		s.State, s.Detail = "sign_in", reason
	case seen.State == "available" || signIn == accounts.SignedIn:
		s.State, s.Detail = "available", reason
		if seen.State == "available" && signIn != accounts.SignedIn {
			s.Detail = "Available"
		}
	default:
		s.State, s.Detail = "unverified", reason
	}
	return s
}

var stateRank = map[string]int{"available": 0, "unverified": 1, "limited": 2, "billing": 3, "sign_in": 4}

// better orders subscriptions for picking: usable first (known to be
// available before merely not known to be limited), a private subscription
// before the CLI's own sign-in, the soonest reset among limited ones.
func better(a, b Subscription) bool {
	if stateRank[a.State] != stateRank[b.State] {
		return stateRank[a.State] < stateRank[b.State]
	}
	if a.Managed != b.Managed {
		return a.Managed
	}
	if a.State == "limited" && a.ResetAt != b.ResetAt {
		return a.ResetAt != 0 && (b.ResetAt == 0 || a.ResetAt < b.ResetAt)
	}
	return a.ProfileID < b.ProfileID
}

// Build combines programs and subscriptions into the report, summarizing
// each provider in one state. ACP agents follow Claude and Codex, in the
// order agents lists them.
func Build(programs map[string]Program, subscriptions map[string][]Subscription, checkedAt time.Time, agents ...acp.Agent) Report {
	report := Report{CheckedAt: checkedAt}
	for _, provider := range Providers {
		program := programs[provider]
		subs := subscriptions[provider]
		p := Provider{Provider: provider, Title: Titles[provider], Program: program, Subscriptions: subs, State: summary(program, subs), Models: models(provider, program)}
		if !program.Installed {
			p.InstallCommand = InstallCommands[provider]
		}
		report.Providers = append(report.Providers, p)
	}
	for _, a := range agents {
		provider := acp.Provider(a.ID)
		program, probed := programs[provider]
		if !probed {
			// Added since the last probe: found now, versioned on the next one.
			program = Probe{LookPath: exec.LookPath}.agentProgram(context.Background(), a)
		}
		subs := subscriptions[provider]
		p := Provider{Provider: provider, Title: a.Title, Program: program, Subscriptions: subs, State: summary(program, subs), ACP: &ACPAgent{ID: a.ID, Icon: a.Icon, Command: a.Command, Args: append([]string(nil), a.Args...), Env: a.EnvNames(), Homepage: a.Homepage, SharesCredentialsWith: a.SharesCredentialsWith, InheritedEnv: a.InheritedEnvNames(), SignIn: a.SignIn, CredentialsNote: a.CredentialsNote, Custom: a.Custom, LaunchProblem: program.LaunchProblem}}
		if !program.Installed {
			p.InstallCommand = a.Install
		}
		report.Providers = append(report.Providers, p)
	}
	return report
}

func summary(program Program, subs []Subscription) string {
	switch {
	case !program.Installed:
		return "not_installed"
	case program.Error != "":
		return "unusable"
	case len(subs) == 0:
		return "no_subscription"
	}
	// Subscriptions are best first, so the first says the most hopeful state.
	switch subs[0].State {
	case "available", "unverified":
		return "ready"
	case "limited":
		return "limited"
	case "billing":
		return "billing"
	}
	return "sign_in"
}

// Cache holds the last probe of the programs; one probe runs at a time.
type Cache struct {
	Probe    Probe
	mu       sync.Mutex
	programs map[string]Program
	checked  time.Time
}

// Cached returns the last probe without probing; nil when there was none.
func (c *Cache) Cached() map[string]Program {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.programs == nil {
		return nil
	}
	copied := make(map[string]Program, len(c.programs))
	for k, v := range c.programs {
		copied[k] = v
	}
	return copied
}

// Programs returns the cached probe, probing first when there is none or
// refresh asks for a new one.
func (c *Cache) Programs(ctx context.Context, refresh bool) (map[string]Program, time.Time) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.programs == nil || refresh || time.Since(c.checked) >= 6*time.Hour {
		c.programs = c.Probe.Programs(ctx)
		c.checked = time.Now()
	}
	copied := make(map[string]Program, len(c.programs))
	for k, v := range c.programs {
		copied[k] = v
	}
	return copied, c.checked
}
