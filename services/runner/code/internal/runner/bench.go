package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/assistant"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/availability"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/docstool"
	"log"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/acp"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/claude"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/codex"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/attachments"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/browser"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/preferences"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/repositories"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/repositorycatalog"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/snippets"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

type Gateway struct {
	agent.Gateway
	Provider string `json:"provider"`
}
type Config struct {
	// Zero selects bounded defaults (32 agents, 128 commands), never unlimited.
	MaxLiveAgents     int                       `json:"maxLiveAgents,omitempty"`
	MaxLiveCommands   int                       `json:"maxLiveCommands,omitempty"`
	SnippetPacks      []string                  `json:"snippetPacks,omitempty"`
	SnippetCommand    string                    `json:"snippetCommand,omitempty"`
	SnippetDirectory  string                    `json:"snippetDirectory,omitempty"`
	RepositoryCatalog repositorycatalog.Options `json:"repositoryCatalog,omitempty"`
	MeterPrices       telemetry.RateCard        `json:"meterPrices,omitempty"`
	MeterUpstreams    telemetry.Upstreams       `json:"meterUpstreams,omitempty"`
	// SubscriptionState owns only managed account enrollment and provider homes.
	// Execution/history state remains private to this Runner.
	SubscriptionState string                `json:"subscriptionState,omitempty"`
	GitHubApp         *githubconnection.App `json:"githubApp,omitempty"`
	SessionStore      sessionstore.Store    `json:"-"`
	// Address is the URL agents' CLI reaches this Runner at. Each execution
	// gets its own endpoint file with it and a credential scoped to that
	// execution; agents never see the owner's endpoint file or bearer.
	Address string `json:"-"`
	State   string `json:"-"`
	// HostRoot holds the session hosts that keep agents alive across Runner
	// restarts. Empty selects <State>/hosts.
	HostRoot string `json:"-"`
	// CommandHostRoot holds the session hosts of managed commands, which
	// outlive the Runner too. Empty selects <State>/command-hosts.
	CommandHostRoot string `json:"-"`
	Worker          string `json:"worker"`
	// BrowserWorker is the agent browser service's main.js (workers/browser).
	// Empty: this Runner offers agents no browser.
	BrowserWorker string `json:"browserWorker,omitempty"`
	// CLIDir holds the lazybox CLI. It is put first on every agent's PATH and
	// enables the command-output briefing. Empty leaves both off.
	CLIDir string `json:"-"`
	// OutputRoot holds each execution's full-output files from `lazybox run`,
	// deleted with its conversation. Empty selects <State>/command-output.
	OutputRoot string `json:"-"`
	// OutputFallbackRoot is where the CLI saves them when a sandbox keeps it
	// out of OutputRoot. Empty selects <temp>/lazybox-output, the CLI's own default.
	OutputFallbackRoot string `json:"-"`
	// ProcessFile is the user's orchestration process file: their defaults
	// and the ceiling no repository process may exceed (internal/process).
	// Empty selects <State>/process.yaml, so each environment has its own.
	ProcessFile string `json:"processFile,omitempty"`
	// ActionsFile is the user's actions and hooks file (POWER.md). Empty
	// selects <State>/actions.yaml; the Dev and Working launchers pass the
	// person's own (~/.config/lazybox/actions.yaml), shared by both. The
	// config files (keys.yaml, settings.yaml, layouts/) live in its folder.
	ActionsFile string `json:"actionsFile,omitempty"`
	// ThemeDirectory is the user's themes folder for Settings → Appearance:
	// .json and .toml files parsed on every ListThemes (internal/themes).
	// Empty selects <State>/themes, so each environment has its own.
	ThemeDirectory string `json:"themeDirectory,omitempty"`
	// OutputBudget bounds both roots together; the oldest files go first.
	// Zero selects 1 GiB.
	OutputBudget int64 `json:"commandOutputBudget,omitempty"`
	// DocsDirectory is the Lazybox documentation (docs) the help assistant
	// reads through its documentation tools. An installed release ships it;
	// the Dev and Working launchers name their source's. Empty: none.
	DocsDirectory string `json:"docsDirectory,omitempty"`
	CodexBinary   string `json:"codexBinary,omitempty"`
	ClaudeBinary  string `json:"claudeBinary,omitempty"`
	// ACPLookPath finds the built-in ACP agents (Gemini CLI, Goose…, ACP.md)
	// on the Runner's PATH; the service sets exec.LookPath. Nil offers only
	// the agents the person added, so what a test machine has installed
	// never changes a test's providers.
	ACPLookPath func(string) (string, error) `json:"-"`
	// Node runs the Claude SDK worker. Empty is `node` on the Runner's PATH
	// (Dev, Working); an installed release names the Node it ships
	// (INSTALL.md), so a launchd service with a minimal PATH still finds one.
	Node            string                   `json:"node,omitempty"`
	Profiles        []accounts.Profile       `json:"profiles"`
	Gateways        []Gateway                `json:"gateways,omitempty"`
	Defaults        accounts.Options         `json:"defaults"`
	DefaultGateways map[string]string        `json:"defaultGateways,omitempty"`
	EventBudget     int64                    `json:"eventBudget,omitempty"`
	CommandBudget   int64                    `json:"commandBudget,omitempty"`
	WorktreeRoot    string                   `json:"worktreeRoot,omitempty"`
	Repositories    []collections.Repository `json:"repositories,omitempty"`

	// HomeCheckoutRoot is where a repository's home checkout goes by default
	// (<root>/<owner>/<name>, homes.go). Empty selects ~/lazybox/repos.
	// Nothing is created there until the person asks for a home.
	HomeCheckoutRoot string `json:"homeCheckoutRoot,omitempty"`

	// EventBudget (above) is the size of one segment of a conversation's
	// event journal: a journal that reaches it continues in a new segment.
	// EventHistoryBudget bounds every segment of one attempt's journal
	// together; zero selects 2 GiB. Past it, events are no longer recorded
	// and the conversation shows the fault; its agent keeps running.
	EventHistoryBudget int64 `json:"eventHistoryBudget,omitempty"`

	// TerminalHostRoot holds the session hosts of workspace terminals (a
	// person's shells). Empty selects <State>/terminal-hosts.
	TerminalHostRoot string `json:"-"`
	// MaxLiveTerminals bounds running terminals; zero selects 16.
	MaxLiveTerminals int `json:"maxLiveTerminals,omitempty"`
	// Processes selects how agents, commands and terminals are hosted.
	Processes ProcessHosting `json:"processes,omitempty"`
	// CodeCheckpoints bounds the per-turn file checkpoints; zero values select defaults.
	CodeCheckpoints codecheckpoints.Limits `json:"codeCheckpoints,omitempty"`
	// ArtifactPoll is how often tracked artifact files are checked; a change
	// becomes a version once it has been stable for ArtifactDebounce. Zero
	// selects 2s for both.
	ArtifactPoll time.Duration `json:"-"`
	// ItemPoll is how often tracked items are looked at (items.go); zero is
	// the default, negative leaves it to the caller (tests).
	ItemPoll         time.Duration `json:"-"`
	ArtifactDebounce time.Duration `json:"-"`
}

type Execution struct {
	Continuation              sync.Mutex
	activity                  sync.Mutex
	Engine                    SessionRuntime
	ID, WorkID, Previous, Cwd string
	CollectionID              string
	Purpose                   string
	Profile                   accounts.Profile
	Options                   accounts.Options
	Env                       []string
	spool                     *journal.Journal
	Saved                     sessionstore.Record
	Archived                  bool
	RecoveryRequired          bool
	// browserTools: its agent was started with the browser tools (BROWSER.md).
	browserTools bool
}
type Bench struct {
	conversationCosts  sync.Map
	verificationFlow   sync.Map
	verificationLaunch sync.Map
	reviewStarting     sync.Map
	forkMu             sync.Mutex
	pendingDeletions   *pendingDeletions
	RepositoryCatalog  *repositorycatalog.Cache
	Telemetry          telemetry.Store
	Meter              *telemetry.Gateway
	Prices             telemetry.Pricer
	Preferences        *preferences.Store
	// Reviews holds each workspace's inline review comments.
	Reviews     *reviews.Store
	GitHub      *githubconnection.Manager
	GitHubAPI   github.API
	GitHubPulls github.PullRequests
	// GitHubIssues reads issues to start a workspace from one (work_items.go).
	GitHubIssues github.IssueReader
	// GitHubItems is what tracked pull requests read (items.go); nil uses
	// GitHubPulls when it can.
	GitHubItems         GitHubItemsAPI
	RepositoryImporter  *repositories.Importer
	RepositoryDiscovery *repositories.Discovery
	probes              int
	History             sessionstore.Store
	archives            map[string]*Execution
	enrollments         map[string]*Enrollment
	AccountFactory      func(context.Context, accounts.Profile) (NativeAccount, error)
	profileMu           sync.RWMutex
	profiles            []accounts.Profile
	mu                  sync.Mutex
	Config              Config
	Accounts            *accounts.State
	Commands            *commands.Manager
	terminalStart       sync.Mutex
	terminalHandoffs    sync.Map
	Terminals           *terminals.Manager
	Attachments         *attachments.Store
	Collections         *collections.Manager
	// Artifacts holds each workspace's versioned documents (artifacts.go).
	Artifacts *artifacts.Store
	watchers  sync.WaitGroup
	ctx       context.Context
	cancel    context.CancelFunc
	items     map[string]*Execution
	opening   int
	closed    bool
	pending   map[string]bool
	starts    sync.WaitGroup
	closeOnce sync.Once
	closeErr  error
	trimming  atomic.Bool
	trims     sync.WaitGroup
	// ports caches what each managed command listens on (CommandPorts).
	ports portCache
	// Availability is the last probe of the provider programs
	// (provider_availability.go); a test may replace its Probe.
	Availability     *availability.Cache
	ProviderReleases availability.ReleaseCache
	// Adapters is an injection boundary for deterministic conformance tests.
	Adapters map[string]agent.Adapter
	// ACP lists the ACP agents the Runner runs (acp_agents.go).
	ACP *acp.Registry
	// agentKey derives each execution's scoped credential (agent_credentials.go).
	agentKey []byte
	// hosting starts every agent, command and terminal host (hosting.go).
	hosting *hosting
	// Checkpoints records each conversation's files around every turn
	// (code_checkpoints.go); checkpointing counts the ones in progress.
	Checkpoints   *codecheckpoints.Manager
	checkpointing sync.WaitGroup
	// runtime is the workspace runtime: approvals, port blocks, runs
	// (workspace_runtime.go).
	runtime *workspaceRuntime
	// Work holds every mission's event log; Prompts the prompt log (work.go).
	Work    *work.Store
	Prompts *work.Prompts
	// Delivery is the delivery journal: conversation bindings and check observations (delivery.go).
	Delivery *delivery.Store
	// deliveryPending holds the runs whose observation a failed write left unrecorded.
	deliveryMu      sync.Mutex
	deliveryPending map[string]error
	// fingerprints shares a workspace fingerprint between concurrent outcome lookups (delivery_outcome.go).
	fingerprints fingerprintCache
	// snippetCatalog is the catalog last listed, to label snippet messages.
	snippetCatalog atomic.Pointer[[]snippets.Entry]
	// power is the actions file, its hooks and the CLI's window requests (power.go).
	power powerState
	// repositoryRequests are agents' requests for repositories (repository_requests.go).
	repositoryRequests *repositoryRequests
	// conversationStarts are agents' requests to start a conversation (conversation_starts.go).
	conversationStarts *conversationStarts
	// Browser is every conversation's agent browser (browser.go, BROWSER.md).
	Browser *browser.Manager
	// reviewAsks wakes agents waiting for an answer (review_loop.go).
	reviewAsks *reviewAsks
	// agentCLI is the shell integration that keeps this Runner's CLI first on
	// agents' PATH, and the last probe of it (agent_cli.go).
	agentCLI agentCLIState
	// titling is the conversation naming in progress (titles.go).
	titling titling
	// orchestrator serializes spawns and wakes waiting orchestrators (orchestrator.go).
	orchestrator *orchestratorState
	// workItems are the tracked pull requests and their links (items.go).
	workItems itemsState
	// homeMu serializes creating and changing home checkouts (homes.go).
	homeMu sync.Mutex
}

func New(cfg Config) (*Bench, error) {
	if cfg.State == "" || !filepath.IsAbs(cfg.State) {
		return nil, errors.New("explicit absolute test state directory required")
	}
	if cfg.MaxLiveAgents < 0 || cfg.MaxLiveCommands < 0 {
		return nil, errors.New("live agent and command limits must not be negative; zero selects bounded defaults")
	}
	if cfg.MaxLiveAgents == 0 {
		cfg.MaxLiveAgents = 32
	}
	if cfg.MaxLiveCommands == 0 {
		cfg.MaxLiveCommands = 128
	}
	if cfg.SubscriptionState == "" {
		cfg.SubscriptionState = cfg.State
	}
	if cfg.HostRoot == "" {
		cfg.HostRoot = filepath.Join(cfg.State, "hosts")
	}
	if cfg.CommandHostRoot == "" {
		cfg.CommandHostRoot = filepath.Join(cfg.State, "command-hosts")
	}
	if cfg.OutputRoot == "" {
		cfg.OutputRoot = filepath.Join(cfg.State, "command-output")
	}
	if cfg.OutputFallbackRoot == "" {
		cfg.OutputFallbackRoot = filepath.Join(os.TempDir(), "lazybox-output")
	}
	if cfg.OutputBudget == 0 {
		cfg.OutputBudget = 1 << 30
	}
	if !filepath.IsAbs(cfg.OutputRoot) || !filepath.IsAbs(cfg.OutputFallbackRoot) || cfg.OutputBudget < 0 {
		return nil, errors.New("command output roots must be absolute and its budget not negative")
	}
	if cfg.TerminalHostRoot == "" {
		cfg.TerminalHostRoot = filepath.Join(cfg.State, "terminal-hosts")
	}
	if cfg.ProcessFile == "" {
		cfg.ProcessFile = filepath.Join(cfg.State, "process.yaml")
	}
	if cfg.ThemeDirectory == "" {
		cfg.ThemeDirectory = filepath.Join(cfg.State, "themes")
	}
	if err := cfg.Processes.Validate(); err != nil {
		return nil, err
	}
	if cfg.MaxLiveTerminals < 0 {
		return nil, errors.New("live terminal limit must not be negative; zero selects the bounded default")
	}
	if !filepath.IsAbs(cfg.HostRoot) || !filepath.IsAbs(cfg.CommandHostRoot) || !filepath.IsAbs(cfg.TerminalHostRoot) {
		return nil, errors.New("session host roots must be absolute directories")
	}
	if !filepath.IsAbs(cfg.SubscriptionState) {
		return nil, errors.New("subscription state must be an absolute directory")
	}
	if err := cfg.Defaults.Validate(); err != nil {
		return nil, err
	}
	if cfg.EventBudget == 0 {
		cfg.EventBudget = 8 << 20
	}
	if cfg.CommandBudget == 0 {
		cfg.CommandBudget = 8 << 20
	}
	if cfg.EventBudget > 64<<20 || cfg.CommandBudget > 64<<20 {
		return nil, errors.New("per-stream budget must not exceed 64 MiB")
	}
	if cfg.EventHistoryBudget == 0 {
		cfg.EventHistoryBudget = 2 << 30
	}
	if cfg.EventHistoryBudget < cfg.EventBudget || cfg.EventHistoryBudget > 64<<30 {
		return nil, errors.New("event history budget must be at least the event budget and at most 64 GiB")
	}
	if len(cfg.Profiles) == 0 {
		cfg.Profiles = []accounts.Profile{{ID: "codex-default", Provider: "codex", Isolation: "inherited"}, {ID: "claude-default", Provider: "claude", Isolation: "inherited"}}
	}
	enrolled, err := accounts.LoadProfiles(cfg.SubscriptionState)
	if err != nil {
		return nil, err
	}
	cfg.Profiles = append(cfg.Profiles, enrolled...)
	seen := map[string]bool{}
	for _, p := range cfg.Profiles {
		if seen[p.ID] || p.ID == "" || (p.Provider != "codex" && p.Provider != "claude") {
			return nil, errors.New("invalid or duplicate profile")
		}
		seen[p.ID] = true
	}
	seen = map[string]bool{}
	for _, g := range cfg.Gateways {
		if seen[g.ID] || g.ID == "metered" || (g.Provider != "codex" && g.Provider != "claude") {
			return nil, errors.New("invalid or duplicate gateway")
		}
		seen[g.ID] = true
		if err := (accounts.Options{Gateway: &g.Gateway}).Validate(); err != nil {
			return nil, err
		}
	}
	if err := os.MkdirAll(filepath.Join(cfg.State, "sessions"), 0700); err != nil {
		return nil, err
	}
	agentKey, err := loadAgentKey(filepath.Join(cfg.State, "agent-credential.key"))
	if err != nil {
		return nil, err
	}
	// Commands a previous Runner left running are re-attached here, before any
	// execution can start or inspect one.
	hosts := newHosting(cfg.Processes)
	manager, err := commands.NewHostedOn(filepath.Join(cfg.State, "commands"), cfg.CommandBudget, cfg.MaxLiveCommands, cfg.CommandHostRoot, hosts)
	if err != nil {
		return nil, err
	}
	files, err := attachments.New(filepath.Join(cfg.State, "attachments"))
	if err != nil {
		_ = manager.Detach()
		return nil, err
	}
	var groups *collections.Manager
	if cfg.WorktreeRoot != "" {
		groups, err = collections.New(cfg.WorktreeRoot, cfg.Repositories)
	} else if len(cfg.Repositories) > 0 {
		err = errors.New("repositories require an explicit worktreeRoot")
	}
	if err != nil {
		_ = manager.Detach()
		return nil, err
	}
	accountState, err := accounts.OpenState(filepath.Join(cfg.State, "accounts", "availability.json"))
	if err != nil {
		_ = manager.Detach()
		if groups != nil {
			_ = groups.Close()
		}
		return nil, err
	}
	history := cfg.SessionStore
	if history == nil {
		history, err = sessionstore.OpenSQLite(filepath.Join(cfg.State, "sessions.db"))
	}
	if err != nil {
		_ = manager.Detach()
		if groups != nil {
			groups.Close()
		}
		return nil, err
	}
	ctx, cancel := context.WithCancel(context.Background())
	b := &Bench{History: history, archives: map[string]*Execution{}, Config: cfg, profiles: append([]accounts.Profile(nil), cfg.Profiles...), Accounts: accountState, Commands: manager, Attachments: files, Collections: groups, ctx: ctx, cancel: cancel, pending: map[string]bool{}, enrollments: map[string]*Enrollment{}, items: map[string]*Execution{}, agentKey: agentKey, hosting: hosts, Adapters: map[string]agent.Adapter{"codex": codex.Adapter{Binary: cfg.CodexBinary}, "claude": claude.Adapter{Worker: cfg.Worker, Binary: claudeProgram(cfg), Node: cfg.Node}}}
	if b.ACP, err = openACP(cfg.State); err != nil {
		_ = b.Detach()
		return nil, err
	}
	b.profiles = b.withACP(b.profiles)
	b.Availability = &availability.Cache{Probe: availability.DefaultProbe(cfg.Worker, cfg.Node, cfg.ClaudeBinary, cfg.CodexBinary, claudeProgram(cfg))}
	b.Availability.Probe.Agents, b.Availability.Probe.AgentProgram = b.ACPAgents, b.acpProgram
	b.Availability.Probe.Models = programModels
	// Shells a previous Runner left running are re-attached here, like commands.
	if b.Terminals, err = terminals.Open(filepath.Join(cfg.State, "terminals"), cfg.TerminalHostRoot, terminals.Options{MaxLive: cfg.MaxLiveTerminals, Backend: hosts}); err != nil {
		_ = b.Detach()
		return nil, err
	}
	// Re-attached shells reach this Runner through their own credential.
	b.refreshTerminalEndpoints()
	privateStore, err := githubconnection.NewFileStore(filepath.Join(cfg.State, "connections"))
	if err == nil {
		client := github.New()
		b.GitHubAPI, b.GitHubPulls, b.GitHubIssues = client, client, client
		b.GitHub, err = githubconnection.New(privateStore, b.GitHubAPI)
	}
	if err == nil && cfg.GitHubApp != nil {
		_, err = b.GitHub.Configure(*cfg.GitHubApp)
	}
	if err == nil && groups != nil {
		b.RepositoryImporter, err = repositories.New(filepath.Join(cfg.State, "repositories"), groups, repositories.Git{})
		if err == nil {
			// Source copies imported before their origin had a fetch refspec
			// get it (repositories/fetchspec.go). Nothing is fetched.
			if _, migrateErr := b.RepositoryImporter.MigrateOriginFetch(ctx); migrateErr != nil {
				log.Printf("repositories: origin fetch refspec: %v", migrateErr)
			}
		}
	}
	if err == nil {
		var store *repositories.FileCatalogStore
		store, err = repositories.NewCatalogStore(filepath.Join(cfg.State, "repository-discovery"))
		if err == nil {
			b.RepositoryDiscovery, err = repositories.NewDiscovery(store, repositories.DirectoryScanner{})
		}
	}
	if err != nil {
		_ = b.Detach()
		return nil, err
	}
	if b.Checkpoints, err = codecheckpoints.Open(filepath.Join(cfg.State, "code-checkpoints"), cfg.CodeCheckpoints); err != nil {
		_ = b.Detach()
		return nil, err
	}
	if b.runtime, err = openWorkspaceRuntime(filepath.Join(cfg.State, "workspace-runtime")); err != nil {
		_ = b.Detach()
		return nil, err
	}
	b.RepositoryCatalog, err = repositorycatalog.New(repositorycatalog.FileStore{Path: filepath.Join(cfg.State, "repository-catalog.json")})
	if err != nil {
		_ = b.Detach()
		return nil, err
	}
	if err = b.importLegacy(); err != nil {
		_ = b.Detach()
		return nil, err
	}
	basePrices := telemetry.DefaultRates()
	for model, rate := range cfg.MeterPrices {
		basePrices[model] = rate
	}
	b.Preferences, err = preferences.Open(filepath.Join(cfg.State, "runner-settings.json"), basePrices)
	if err != nil {
		_ = b.Detach()
		return nil, err
	}
	b.Reviews, err = reviews.New(filepath.Join(cfg.State, "reviews"))
	if err != nil {
		_ = b.Detach()
		return nil, err
	}
	if b.repositoryRequests, err = openRepositoryRequests(filepath.Join(cfg.State, "repository-requests.json")); err != nil {
		_ = b.Detach()
		return nil, err
	}
	if b.conversationStarts, err = openConversationStarts(filepath.Join(cfg.State, "conversation-starts.json")); err != nil {
		_ = b.Detach()
		return nil, err
	}
	if err = b.openBrowser(); err != nil {
		_ = b.Detach()
		return nil, err
	}
	// The person's choice of backend applies to the next process started.
	preferred := func() string { return b.Preferences.Snapshot().ProcessBackend }
	b.hosting.preferred.Store(&preferred)
	b.Prices = b.Preferences
	if err = b.openWork(); err != nil {
		_ = b.Detach()
		return nil, err
	}
	if b.Artifacts, err = artifacts.Open(filepath.Join(cfg.State, "artifacts")); err != nil {
		_ = b.Detach()
		return nil, err
	}
	b.watchArtifacts()
	b.watchWorkflowReviews()
	b.openPower()
	if err = b.migrateAccounts(); err != nil {
		_ = b.Detach()
		return nil, err
	}
	if err = b.openItems(); err != nil {
		_ = b.Detach()
		return nil, err
	}
	b.Telemetry, err = telemetry.Open(filepath.Join(cfg.State, "telemetry.db"))
	if err == nil {
		b.Meter, err = telemetry.StartGateway(b.Telemetry, b.Prices, cfg.MeterUpstreams)
	}
	if err != nil {
		_ = b.Detach()
		return nil, err
	}
	if b.pendingDeletions, err = openPendingDeletions(filepath.Join(cfg.State, "pending-deletions.json")); err != nil {
		_ = b.Detach()
		return nil, err
	}
	b.watchPendingDeletions()
	b.watchReviewWorkflows()
	return b, nil
}

func (b *Bench) Get(id string) (*Execution, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if ids, err := b.History.Deleted(b.ctx, id); err != nil {
		return nil, err
	} else if len(ids) > 0 {
		return nil, os.ErrNotExist
	}
	if e := b.items[id]; e != nil {
		return e, nil
	}
	if e := b.archives[id]; e != nil {
		return e, nil
	}
	record, err := b.History.Get(b.ctx, id)
	if err != nil {
		return nil, err
	}
	e, err := b.restore(record)
	if err != nil {
		return nil, err
	}
	b.archives[id] = e
	return e, nil
}
func (b *Bench) List() []*Execution {
	b.mu.Lock()
	defer b.mu.Unlock()
	items := make([]*Execution, 0, len(b.items))
	for _, e := range b.items {
		items = append(items, e)
	}
	sort.Slice(items, func(i, j int) bool { return items[i].Saved.CreatedAt.After(items[j].Saved.CreatedAt) })
	return items
}

func (b *Bench) Open(profile, cwd, resume string, options accounts.Options, gatewayID, workID, previous string) (*Execution, error) {
	return b.open(profile, cwd, resume, options, gatewayID, workID, previous, "", agent.Coding, "", false)
}

func (b *Bench) OpenCollection(profile, collectionID, resume string, options accounts.Options, gatewayID, previous string) (*Execution, error) {
	if collectionID == "" {
		return nil, errors.New("collection id required")
	}
	return b.open(profile, "", resume, options, gatewayID, "", previous, collectionID, agent.Coding, "", false)
}

// OpenReviewer starts a reviewer in the workspace it reviews (agent.Reviewer):
// it is reached from its review, and ends once its findings are handed on.
func (b *Bench) OpenReviewer(profile, collectionID string, options accounts.Options) (*Execution, error) {
	if collectionID == "" {
		return nil, errors.New("collection id required")
	}
	return b.open(profile, "", "", options, "", "", "", collectionID, agent.Reviewer, "", false)
}

// OpenEmptyWorkspace starts a coding session in a new Runner-owned directory.
// Closing the session retains its files; no Git repository is initialized.
func (b *Bench) OpenEmptyWorkspace(profile string, options accounts.Options, gatewayID string) (*Execution, error) {
	return b.open(profile, "", "", options, gatewayID, "", "", "", agent.Coding, "", true)
}

// OpenAssistant starts an artifact companion in its workspace, or a bounded
// application assistant with read-only permissions.
func (b *Bench) OpenAssistant(profile, purpose, contextText string, options accounts.Options, gatewayID string, workspace ...string) (*Execution, error) {
	if purpose == agent.Artifact {
		if len(workspace) == 0 || workspace[0] == "" || len(contextText) > 128<<10 {
			return nil, errors.New("artifact companion requires a workspace and context up to 128 KiB")
		}
		return b.open(profile, "", "", options, gatewayID, "", "", workspace[0], purpose, contextText, false)
	}
	if !agent.IsAssistant(purpose) || len(contextText) > 128<<10 {
		return nil, errors.New("assistant purpose must be help, planning, or summary; context max 128 KiB")
	}
	cwd := filepath.Join(b.Config.State, "assistants")
	if err := os.MkdirAll(cwd, 0700); err != nil {
		return nil, err
	}
	options.Permission = "read-only"
	options.Workflow = "execute"
	role := "Answer from the supplied context. Produce advice or a proposal; do not execute commands, edit files, or apply configuration."
	product := assistant.Context
	if purpose == "help" {
		// Only the help assistant has the settings and documentation tools; the user approves each settings change.
		product += "\n" + assistant.Docs(docstool.Index(b.Config.DocsDirectory)) + "\n" + assistant.Settings
		role = "Answer from the documentation and the supplied context. Do not execute commands or edit files. You may propose a change to Lazybox's own settings only through the settings tools, and only the user's approval applies it."
	}
	contextText = product + "\nYou are the Lazybox application assistant for " + purpose + ". " + role + " Supplied context follows:\n" + contextText
	collectionID := ""
	if len(workspace) > 0 {
		collectionID = workspace[0]
	}
	return b.open(profile, cwd, "", options, gatewayID, "", "", collectionID, purpose, contextText, false)
}

func (b *Bench) open(profile, cwd, resume string, options accounts.Options, gatewayID, workID, previous, collectionID, purpose, suppliedContext string, emptyWorkspace bool, fork ...conversationFork) (*Execution, error) {
	id := commands.ID()
	if len(fork) > 0 {
		id = fork[0].id
	}
	return b.openWithID(id, profile, cwd, resume, options, gatewayID, workID, previous, collectionID, purpose, suppliedContext, emptyWorkspace, fork...)
}

func (b *Bench) openWithID(id, profile, cwd, resume string, options accounts.Options, gatewayID, workID, previous, collectionID, purpose, suppliedContext string, emptyWorkspace bool, fork ...conversationFork) (*Execution, error) {
	if err := b.ReloadProfiles(); err != nil {
		return nil, err
	}
	b.mu.Lock()
	activeCount := 0
	for _, item := range b.items {
		snapshot := item.Engine.Snapshot()
		if !snapshot.Closed || snapshot.Fault == "native_exit_unconfirmed" {
			activeCount++
		}
	}
	if b.closed || activeCount+b.opening >= b.Config.MaxLiveAgents {
		b.mu.Unlock()
		return nil, errors.New("runner closed or execution capacity exhausted")
	}
	b.opening++
	b.pending[id] = true
	b.starts.Add(1)
	b.mu.Unlock()
	defer func() { b.mu.Lock(); b.opening--; delete(b.pending, id); b.mu.Unlock(); b.starts.Done() }()
	contextText := suppliedContext
	var parent *sessionstore.Record
	if previous != "" {
		record, err := b.History.Get(b.ctx, previous)
		if err != nil {
			return nil, err
		}
		parent = &record
		// A resume or a rotation continues the same kind of agent (a reviewer,
		// a companion), whichever way it was asked for.
		if record.Purpose != "" && agent.TypeOf(record.Purpose).Workspace {
			purpose = record.Purpose
		}
		if _, err = b.History.Successor(b.ctx, previous); err == nil {
			return nil, errors.New("execution already has a successor")
		} else if !errors.Is(err, os.ErrNotExist) {
			return nil, err
		}
		contextText = record.Context
	}
	if collectionID != "" {
		if b.Collections == nil {
			return nil, errors.New("collections require configured worktreeRoot and repositories")
		}
		c, err := b.Collections.Acquire(b.ctx, collectionID, id, b.Active)
		if err != nil {
			return nil, err
		}
		defer func() {
			b.mu.Lock()
			attached := b.items[id] != nil
			b.mu.Unlock()
			if !attached {
				b.Collections.Release(collectionID, id)
			}
		}()
		cwd, workID = c.Root, c.WorkID
		data, _ := json.Marshal(struct {
			ID, Root string
			Members  []collections.Member
		}{c.ID, c.Root, c.Members})
		workspaceContext := "Runner workspace (paths are on the Runner; branch and baseCommit describe initial provisioning):\n" + string(data) + "\nWork across these member directories. Read each repository's applicable agent instructions and follow the resolved work policies."
		if line := originBriefing(c); line != "" {
			workspaceContext += "\n" + line
		}
		if line := b.homesBriefing(c); line != "" {
			workspaceContext += "\n" + line
		}
		if parent == nil {
			contextText += "\n" + workspaceContext
		}
	}
	var selected accounts.Profile
	for _, p := range b.Profiles() {
		if p.ID == profile {
			selected = p
			break
		}
	}
	if selected.ID == "" {
		return nil, errors.New("unknown account profile")
	}
	if len(fork) > 0 && (selected.Provider != fork[0].provider || !sameAccount(selected, fork[0].profileBinding)) {
		return nil, errors.New("source conversation profile is no longer available")
	}
	if emptyWorkspace {
		root := filepath.Join(b.Config.State, "workspaces")
		if err := os.MkdirAll(root, 0700); err != nil {
			return nil, err
		}
		cwd = filepath.Join(root, id)
		if err := os.Mkdir(cwd, 0700); err != nil {
			return nil, err
		}
		defer func() {
			b.mu.Lock()
			attached := b.items[id] != nil
			b.mu.Unlock()
			// Only remove an empty failed-start directory. Never discard provider/user writes.
			if !attached {
				_ = os.Remove(cwd)
			}
		}()
	}
	if !filepath.IsAbs(cwd) {
		return nil, errors.New("working directory must be absolute")
	}
	info, err := os.Stat(cwd)
	if err != nil {
		return nil, err
	}
	if !info.IsDir() {
		return nil, errors.New("working directory is not a directory")
	}
	if collectionID == "" && b.Collections != nil {
		resolved, err := filepath.EvalSymlinks(cwd)
		if err != nil {
			return nil, err
		}
		for _, c := range b.Collections.List() {
			rel, err := filepath.Rel(c.Root, resolved)
			if err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
				return nil, errors.New("working directory belongs to a Collection; attach using collection_id")
			}
		}
	}
	var briefing []briefingSection
	if parent == nil {
		prefs := b.Preferences.Snapshot()
		// The briefing's sections, each at the version recorded below.
		briefing = b.briefingSections(selected.Provider, purpose, collectionID, b.browserFor(selected.Provider, purpose, collectionID))
		if options.Instructions == "" {
			options.Instructions = b.Config.Defaults.Instructions
		}
		contextText += "\n" + sectionText(briefing, "settings") + "\n" + sectionText(briefing, "provider") + "\n" + options.Instructions
		workspaceAgent := agent.TypeOf(purpose).Workspace
		if options.BehaviorProfile == "" && workspaceAgent {
			options.BehaviorProfile = b.Config.Defaults.BehaviorProfile
		}
		if behavior := options.BehaviorInstructions(); workspaceAgent && behavior != "" {
			contextText += "\n\n" + behavior
		}
		var rest []string
		for _, s := range briefing {
			if s.id != "settings" && s.id != "provider" {
				rest = append(rest, s.text)
			}
		}
		if len(rest) > 0 {
			contextText += "\n" + strings.Join(rest, "\n\n")
		}
		if options.Workflow == "" {
			options.Workflow = b.Config.Defaults.Workflow
		}
		if options.Permission == "" && prefs.Permission != "" {
			options.Permission = prefs.Permission
			if options.Workflow == "plan" {
				options.Permission = "read-only"
			}
		}
		policy, switching := b.AccountPolicy(selected.Provider)
		options.RotationProfiles = rotationPool(options, b.Config.Defaults.RotationProfiles, b.Profiles(), selected, prefs.RotationExcluded, policy, switching)
		options.Gateway = nil
		if options.Model == "" {
			options.Model = b.Config.Defaults.Model
		}
		if options.Permission == "" {
			options.Permission = b.Config.Defaults.Permission
		}
		if workspaceAgent && options.BehaviorProfile == accounts.CheckInFirst {
			if selected.Provider != "codex" && selected.Provider != "claude" {
				return nil, errors.New("Check in first requires a harness with enforced read-only access (Claude or Codex)")
			}
			options.Permission = "read-only"
		}
	}
	if gatewayID == "" {
		gatewayID = b.Config.DefaultGateways[selected.Provider]
	}
	// Resolve every attempt from the selected ID, including an explicit Direct
	// override of a metered parent. Never reuse its old process route.
	options.Gateway = nil
	if gatewayID == "metered" {
		options.Gateway = &agent.Gateway{ID: "metered", URL: b.Meter.BaseURL}
	}
	if gatewayID != "" && gatewayID != "direct" && gatewayID != "metered" {
		for _, g := range b.Config.Gateways {
			if g.ID == gatewayID && g.Provider == selected.Provider {
				gateway := g.Gateway
				options.Gateway = &gateway
			}
		}
		if options.Gateway == nil {
			return nil, errors.New("unknown gateway for selected provider")
		}
	}
	if err := agent.ValidEffort(selected.Provider, options.Effort); err != nil {
		return nil, err
	}
	if options.Model, err = availability.ResolveModel(selected.Provider, options.Model); err != nil {
		return nil, err
	}
	if err = options.Validate(); err != nil {
		return nil, err
	}
	if err = b.claudeSignInAllowed(selected); err != nil {
		return nil, err
	}
	env, err := b.profileEnv(selected)
	if err != nil {
		return nil, err
	}
	if options.Gateway != nil && options.Gateway.TokenEnv != "" {
		found := false
		for _, entry := range env {
			if len(entry) > len(options.Gateway.TokenEnv)+1 && entry[:len(options.Gateway.TokenEnv)+1] == options.Gateway.TokenEnv+"=" {
				found = true
			}
		}
		if !found {
			return nil, errors.New("gateway credential environment reference unavailable")
		}
	}
	env = b.executionEnv(env, id, collectionID)
	if err = b.writeAgentEndpoint(id); err != nil {
		return nil, err
	}
	defer func() {
		b.mu.Lock()
		attached := b.items[id] != nil
		b.mu.Unlock()
		if !attached {
			_ = b.removeAgentEndpoint(id)
		}
	}()
	if workID == "" {
		workID = id
	}
	spool, err := b.openJournal(id)
	if err != nil {
		return nil, err
	}
	e := &Execution{ID: id, WorkID: workID, Previous: previous, Cwd: cwd, CollectionID: collectionID, Purpose: purpose, Profile: selected, Options: options, Env: env, spool: spool}
	e.browserTools = b.browserOffered(e)
	conversationID := id
	if parent != nil {
		conversationID = parent.ConversationID
	}
	adapter := b.sessionAdapter(e, selected, options, conversationID)
	record := sessionstore.Record{ID: id, ConversationID: conversationID, Previous: previous, WorkID: workID, CollectionID: collectionID, Cwd: cwd, Purpose: purpose, Context: contextText, ProfileID: profile, Provider: selected.Provider, ProfileBinding: profileBinding(selected), Options: options, GatewayID: b.GatewayID(e), Title: filepath.Base(cwd), CreatedAt: time.Now().UTC(), Snapshot: engine.Snapshot{ID: id, Profile: profile, Purpose: purpose, Reference: resume}}
	if parent != nil {
		// A successor continues the conversation, including updates it already
		// received: it has the briefing its predecessor had.
		record.Instructions = parent.Instructions
		record.Briefing = parent.Briefing
	} else {
		record.Briefing = briefingVersions(briefing)
	}
	offered := e.browserTools
	record.BrowserTools = &offered
	record.ProfileBindings = map[string]string{}
	for _, p := range b.Profiles() {
		record.ProfileBindings[p.ID] = profileBinding(p)
	}
	if parent != nil {
		record.ForkedFrom, record.ForkCursor = parent.ForkedFrom, parent.ForkCursor
		record.ForkThread, record.ForkTurn = parent.ForkThread, parent.ForkTurn
		record.Title = parent.Title
		if parent.Snapshot.Title != "" {
			record.Title = parent.Snapshot.Title
		}
		record.Snapshot.Capabilities = parent.Snapshot.Capabilities
		record.ProviderSettings = parent.ProviderSettings
		if parent.Provider == selected.Provider {
			record.Snapshot.Grants = parent.Snapshot.Grants
		}
	}
	var inherited []agent.Event
	if len(fork) > 0 {
		record.ForkedFrom = fork[0].source
		record.ForkCursor = fork[0].cursor
		record.ForkThread, record.ForkTurn = fork[0].thread, fork[0].turn
		inherited = fork[0].history
	}
	e.Saved = record
	if err = b.History.Create(b.ctx, record); err != nil {
		spool.Dispose()
		return nil, err
	}
	open := b.sessionOpen(e, contextText, resume)
	if len(fork) > 0 {
		open.ForkThread = fork[0].thread
		open.ForkAfterTurn = fork[0].turn
	}
	if parent != nil && profile == parent.Snapshot.Profile {
		open.ProviderSettings = parent.ProviderSettings
	}
	// The person's "always allow" choices go on with the conversation: its
	// new process holds them, and the menu still lists them.
	open.Grants = record.Snapshot.Grants
	eng, err := engine.NewDurable(b.ctx, adapter, open, spool, func(snapshot engine.Snapshot, settings json.RawMessage) error {
		return b.History.SaveState(context.Background(), id, snapshot, settings)
	}, func(a engine.Attachment) error { return b.saveAttachment(id, a) }, inherited...)
	if err != nil {
		snapshot := record.Snapshot
		snapshot.Closed = true
		snapshot.Fault = "provider startup failed"
		snapshot.Status = agent.Failed
		saveErr := b.History.SaveState(context.Background(), id, snapshot, record.ProviderSettings)
		spool.Dispose()
		return nil, errors.Join(err, saveErr)
	}
	e.Engine = eng
	b.watchTurns(e)
	if _, err = spool.Append(map[string]any{"type": "execution_context", "workId": workID, "collectionId": collectionID, "previousExecutionId": previous, "cwd": cwd, "profile": profile, "model": options.Model, "permission": options.Permission, "conversationId": conversationID}); err != nil {
		eng.Close()
		return nil, err
	}
	b.mu.Lock()
	if b.closed {
		b.mu.Unlock()
		eng.Close()
		return nil, errors.New("runner closed during session startup")
	}
	b.items[id] = e
	b.mu.Unlock()
	if parent == nil {
		b.announceAgentCLI(e)
	}
	return e, nil
}

func (b *Bench) Start(e *Execution, title string, argv []string) (*commands.Command, error) {
	return b.start(e, title, argv, nil, false, "")
}

// StartForPerson starts a command the person confirmed in a conversation
// (StartCommand, which only the owner may call). A read-only or plan
// conversation does not refuse it: that mode bounds what the agent may do,
// and the person could run the same command in a workspace terminal. An
// assistant conversation has no workspace and still refuses. cwd, when set,
// must be a directory inside the conversation's folder; empty is that folder.
func (b *Bench) StartForPerson(e *Execution, title string, argv []string, cwd string) (*commands.Command, error) {
	return b.start(e, title, argv, nil, true, cwd)
}

// commandDir resolves the folder a person's command runs in: the
// conversation's own folder, or a directory inside it, symlinks resolved so a
// link cannot lead outside.
func commandDir(root, cwd string) (string, error) {
	if cwd == "" {
		return root, nil
	}
	if !filepath.IsAbs(cwd) {
		return "", errors.New("the command folder must be an absolute path")
	}
	base, err := filepath.EvalSymlinks(root)
	if err != nil {
		return "", fmt.Errorf("the conversation's folder cannot be resolved: %w", err)
	}
	dir, err := filepath.EvalSymlinks(filepath.Clean(cwd))
	if err != nil {
		return "", fmt.Errorf("the command folder does not exist: %w", err)
	}
	if rel, err := filepath.Rel(base, dir); err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return "", errors.New("the command folder must be inside the conversation's workspace")
	}
	if info, err := os.Stat(dir); err != nil || !info.IsDir() {
		return "", errors.New("the command folder is not a directory")
	}
	return dir, nil
}

func (b *Bench) start(e *Execution, title string, argv []string, pinnedEnv []string, person bool, cwd string) (*commands.Command, error) {
	e.activity.Lock()
	defer e.activity.Unlock()
	if agent.IsAssistant(e.Purpose) || (!person && (e.Options.Permission == "read-only" || e.Options.Workflow == "plan")) {
		return nil, errors.New("managed command start is disabled for read-only executions")
	}
	dir, err := commandDir(e.Cwd, cwd)
	if err != nil {
		return nil, err
	}
	if e.Engine != nil {
		s := e.Engine.Snapshot()
		if s.Closed || s.Fault != "" || s.Status == agent.Switching {
			return nil, errors.New("execution is closed or faulted")
		}
	}
	env := e.Env
	if e.Engine != nil {
		id := e.Engine.Snapshot().Profile
		if id != e.Profile.ID {
			for _, p := range b.Profiles() {
				if p.ID == id {
					var err error
					env, err = b.profileEnv(p)
					if err != nil {
						return nil, err
					}
					env = b.executionEnv(env, e.ID, e.CollectionID)
				}
			}
		}
	}
	if pinnedEnv != nil {
		env = pinnedEnv
	}
	return b.Commands.Start(commands.Spec{SessionID: e.ID, Title: title, Cwd: dir, Argv: argv, Env: env})
}

// PipeCheck is a run's --check (or, Prerequisite, --needs) criterion.
type PipeCheck struct {
	Criterion    string
	Prerequisite bool
}

func (b *Bench) RegisterPipe(e *Execution, title string, createsPullRequest bool, check *PipeCheck) (*commands.Command, error) {
	e.activity.Lock()
	defer e.activity.Unlock()
	if e.Engine.Snapshot().Closed {
		return nil, errors.New("execution closed")
	}
	spec := commands.Spec{SessionID: e.ID, Title: title, Cwd: e.Cwd, CreatesPullRequest: createsPullRequest}
	if createsPullRequest {
		spec.DeliveryAttempt = b.deliveryAttempt(conversationOf(e))
	}
	if check != nil {
		resolved, err := b.resolveCheck(e, check.Criterion, check.Prerequisite)
		if err != nil {
			return nil, err
		}
		spec.Check = resolved
	}
	return b.Commands.Register(spec)
}

// withCommandEnv is a command's environment with the variables an agent asked
// to add. The conversation's own credential and endpoint stay as they are.
func withCommandEnv(base, pinned, extra []string) ([]string, error) {
	if pinned != nil {
		base = pinned
	}
	if len(extra) > 64 {
		return nil, errors.New("at most 64 added environment variables")
	}
	set := map[string]string{}
	order := []string{}
	for _, entry := range base {
		k, v, _ := strings.Cut(entry, "=")
		if _, seen := set[k]; !seen {
			order = append(order, k)
		}
		set[k] = v
	}
	for _, entry := range extra {
		k, v, ok := strings.Cut(entry, "=")
		if !ok || k == "" || strings.ContainsAny(k, "\x00") || strings.ContainsRune(v, 0) {
			return nil, errors.New("environment entries are NAME=value")
		}
		if strings.HasPrefix(k, "LAZYBOX_") {
			return nil, fmt.Errorf("%s belongs to Lazybox and cannot be set by an agent", k)
		}
		if _, seen := set[k]; !seen {
			order = append(order, k)
		}
		set[k] = v
	}
	out := make([]string, 0, len(order))
	for _, k := range order {
		out = append(out, k+"="+set[k])
	}
	return out, nil
}

func (b *Bench) toolWithEnv(ctx context.Context, e *Execution, name string, args json.RawMessage, pinnedEnv []string) (string, error) {
	if err := ctx.Err(); err != nil {
		return "", err
	}
	if agent.BrowserTool(name) {
		return b.browserTool(ctx, e, name, args)
	}
	if name == agent.ReviewFindingTool {
		var request FindingRequest
		if err := json.Unmarshal(args, &request); err != nil {
			return "", err
		}
		finding, updated, run, err := b.AddReviewFinding(ctx, e.ID, request)
		if err != nil {
			return "", err
		}
		data, err := json.Marshal(map[string]any{"findingId": finding.ID, "reviewId": run.ID, "updated": updated})
		return string(data), err
	}
	if name != "lazybox_command" {
		return "", agent.ErrUnsupported
	}
	var p struct {
		AgentInput bool     `json:"agentInput"`
		Input      string   `json:"input"`
		Action     string   `json:"action"`
		Argv       []string `json:"argv"`
		ID         string   `json:"id"`
		Offset     uint64   `json:"offset"`
		Name       string   `json:"name"`
		Restart    bool     `json:"restart"`
		Member     string   `json:"member"`
		Expected   string   `json:"expected"`
		Regression string   `json:"regression"`
		// A start's console title, folder (inside the workspace) and added
		// environment: what an ACP agent's terminal/create asks for.
		Title string   `json:"title"`
		Cwd   string   `json:"cwd"`
		Env   []string `json:"env"`
		// Limit bounds a tail, in bytes.
		Limit int `json:"limit"`
	}
	if err := json.Unmarshal(args, &p); err != nil {
		return "", err
	}
	var value any
	if p.Action == agent.InteractiveCommandAction {
		v, err := b.requestTerminalHandoff(ctx, e, p.Argv, p.Cwd, pinnedEnv, p.AgentInput)
		if err != nil {
			return "", err
		}
		value = v
	} else if p.Action == "interactive_read" || p.Action == "interactive_write" {
		result, err := b.interactiveTerminalIO(ctx, e, p.Action, p.ID, p.Input, p.Offset)
		if err != nil {
			return "", err
		}
		value = result
	} else if p.Action == "verify" {
		v, err := b.startVerification(ctx, e, reviews.VerificationCheck{Name: p.Name, Member: p.Member, Expected: p.Expected, Regression: p.Regression, Argv: p.Argv})
		if err != nil {
			return "", err
		}
		value = v
	} else if p.Action == "actions" || p.Action == "start_action" {
		v, err := b.agentAction(ctx, e, p.Action, p.Name, p.Restart)
		if err != nil {
			return "", err
		}
		value = v
	} else if p.Action == "start" {
		title := p.Title
		if title == "" || len(title) > 512 {
			title = "agent command"
		}
		env := pinnedEnv
		if len(p.Env) > 0 {
			var err error
			if env, err = withCommandEnv(e.Env, pinnedEnv, p.Env); err != nil {
				return "", err
			}
		}
		c, err := b.start(e, title, p.Argv, env, false, p.Cwd)
		if err != nil {
			return "", err
		}
		value = c.Status()
	} else {
		c, err := b.Commands.Get(p.ID)
		if err != nil {
			return "", err
		}
		spec := c.Status().Spec
		owned := spec.SessionID != "" && (spec.SessionID == e.ID || spec.SessionID == e.Previous)
		workspaceAction := e.CollectionID != "" && spec.CollectionID == e.CollectionID && spec.Kind == "action"
		reading := p.Action == "read" || p.Action == "tail" || p.Action == "wait"
		if !owned && !(workspaceAction && reading) {
			return "", errors.New("command belongs to another execution")
		}
		switch p.Action {
		case "read":
			data, status, err := c.Read(ctx, p.Offset, 32<<10)
			if err != nil {
				return "", err
			}
			value = map[string]any{"output": string(data), "nextOffset": p.Offset + uint64(len(data)), "status": status}
		case "tail":
			// The latest output, at most limit bytes on a UTF-8 boundary, and
			// whether earlier output was left out (ACP's terminal/output).
			limit := p.Limit
			if limit <= 0 || limit > 1<<20 {
				limit = 1 << 20
			}
			data := c.Tail(ctx, limit+4)
			status := c.Status()
			truncated := status.Offset > uint64(len(data)) || len(data) > limit
			for len(data) > limit || (len(data) > 0 && !utf8.RuneStart(data[0])) {
				data = data[1:]
			}
			value = map[string]any{"output": string(data), "truncated": truncated, "status": status}
		case "wait":
			status, err := c.Wait(ctx)
			if err != nil {
				return "", err
			}
			value = status
		case "cancel":
			if err := c.Cancel(); err != nil {
				return "", err
			}
			value = c.Status()
		default:
			return "", errors.New("unknown managed command action")
		}
	}
	data, err := json.Marshal(value)
	return string(data), err
}

// Close stops every agent this Runner supervises. Use Detach when the Runner is
// only going away (shutdown, upgrade, hot reload) and its agents should stay.
func (b *Bench) Close() error { return b.shutdown(false) }

// Detach shuts the Runner down but leaves every re-attachable agent and every
// running managed command in its session host for the next Runner to adopt. An
// agent that cannot outlive the Runner is stopped as before.
func (b *Bench) Detach() error { return b.shutdown(true) }

// Stopping reports whether this Runner is shutting down. Its journals close
// under their readers then, but a live conversation goes on in the next
// Runner: the end of a reader's stream is not the end of the conversation.
func (b *Bench) Stopping() bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.closed
}

func (b *Bench) shutdown(detach bool) error {
	b.closeOnce.Do(func() {
		b.mu.Lock()
		b.closed = true
		b.mu.Unlock()
		// Detach before cancelling the Runner context: a cancelled local pipe
		// stops its agent. A start still in progress is cancelled below and
		// closes its own engine once it sees the Runner closed.
		if detach {
			for _, e := range b.List() {
				live, ok := e.Engine.(*engine.Engine)
				if !ok || !live.Detachable() {
					continue
				}
				if err := live.Detach(); err != nil {
					b.closeErr = errors.Join(b.closeErr, fmt.Errorf("execution %s could not detach; stopping it: %w", e.ID, err))
				}
			}
		}
		b.cancel()
		if b.power.Extensions != nil {
			// Open panels lose their pages and credentials with the Runner.
			b.power.Extensions.Close()
		}
		if b.Browser != nil {
			// The agent browser is the Runner's: it closes with it (BROWSER.md).
			b.closeErr = errors.Join(b.closeErr, b.Browser.Close())
		}
		if b.GitHub != nil {
			b.GitHub.Close()
		}
		b.starts.Wait()
		b.trims.Wait()
		b.agentCLI.stop()
		b.checkpointing.Wait()
		b.watchers.Wait()
		for _, e := range b.List() {
			// A no-op for a detached engine: its agent is the next Runner's.
			b.closeErr = errors.Join(b.closeErr, e.Engine.Close())
		}
		if detach {
			b.closeErr = errors.Join(b.closeErr, b.Commands.Detach())
		} else {
			b.closeErr = errors.Join(b.closeErr, b.Commands.Close())
		}
		if b.Terminals != nil {
			if detach {
				b.closeErr = errors.Join(b.closeErr, b.Terminals.Detach())
			} else {
				b.closeErr = errors.Join(b.closeErr, b.Terminals.Close())
			}
		}
		if b.Collections != nil {
			for _, e := range b.List() {
				if e.CollectionID != "" && !b.Active(e.ID) {
					b.closeErr = errors.Join(b.closeErr, b.Collections.Release(e.CollectionID, e.ID))
				}
			}
			b.closeErr = errors.Join(b.closeErr, b.Collections.Close())
		}

		for _, e := range b.List() {
			b.closeErr = errors.Join(b.closeErr, e.spool.Dispose())
		}
		for _, e := range b.archives {
			b.closeErr = errors.Join(b.closeErr, e.spool.Dispose())
		}
		b.closeErr = errors.Join(b.closeErr, b.History.Close())
		b.closeErr = errors.Join(b.closeErr, b.closeWork())
		if b.Meter != nil {
			b.closeErr = errors.Join(b.closeErr, b.Meter.Close())
		}
		if b.Telemetry != nil {
			b.closeErr = errors.Join(b.closeErr, b.Telemetry.Close())
		}
	})
	return b.closeErr
}

// Active keeps collection ownership through startup and through commands that
// survive closing the agent. The activity mutex closes the start/lease race.
func (b *Bench) Active(id string) bool {
	b.mu.Lock()
	pending, e := b.pending[id], b.items[id]
	b.mu.Unlock()
	if pending {
		return true
	}
	if e == nil {
		return false
	}
	e.activity.Lock()
	defer e.activity.Unlock()
	if state := e.Engine.Snapshot(); !state.Closed || state.Fault == "native_exit_unconfirmed" {
		return true
	}
	for _, c := range b.Commands.List(id) {
		if !c.Done || c.CleanupUnconfirmed() {
			return true
		}
	}
	return false
}

func (b *Bench) Record(e *Execution, value any) error { _, err := e.spool.Append(value); return err }
func (b *Bench) GatewayID(e *Execution) string {
	if e.Options.Gateway == nil {
		return "direct"
	}
	return e.Options.Gateway.ID
}

// Profiles returns a snapshot of the live account registry.
func (b *Bench) Profiles() []accounts.Profile {
	b.profileMu.RLock()
	defer b.profileMu.RUnlock()
	profiles := append([]accounts.Profile(nil), b.profiles...)
	for i, p := range profiles {
		if p.EnvRefs != nil {
			profiles[i].EnvRefs = map[string]string{}
			for target, ref := range p.EnvRefs {
				profiles[i].EnvRefs[target] = ref
			}
		}
	}
	return profiles
}
func (b *Bench) ReloadProfiles() error {
	b.profileMu.Lock()
	defer b.profileMu.Unlock()
	enrolled, err := accounts.LoadProfiles(b.Config.SubscriptionState)
	if err != nil {
		return err
	}
	next := append([]accounts.Profile(nil), b.profiles...)
	byID := map[string]int{}
	for i, p := range next {
		byID[p.ID] = i
	}
	for _, p := range enrolled {
		if i, exists := byID[p.ID]; exists {
			old := next[i]
			if old.Provider != p.Provider || old.Home != p.Home || old.TokenFile != p.TokenFile {
				return errors.New("enrolled profile conflicts with configured identity")
			}
			if old.Managed {
				next[i] = p
			}
			continue
		}
		byID[p.ID] = len(next)
		next = append(next, p)
	}
	b.profiles = b.withACP(next)
	return nil
}
