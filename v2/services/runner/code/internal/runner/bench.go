package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/assistant"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/claude"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/codex"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/attachments"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/preferences"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/repositories"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/repositorycatalog"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/sessionstore"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
	"github.com/AntoineToussaint/lazybox/v2/internal/sources/github"
)

type Gateway struct {
	agent.Gateway
	Provider string `json:"provider"`
}
type Config struct {
	// Zero selects bounded defaults (32 agents, 128 commands), never unlimited.
	MaxLiveAgents     int                       `json:"maxLiveAgents,omitempty"`
	MaxLiveCommands   int                       `json:"maxLiveCommands,omitempty"`
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
	Endpoint          string                `json:"-"`
	State             string                `json:"-"`
	// HostRoot holds the session hosts that keep agents alive across Runner
	// restarts. Empty selects <State>/hosts.
	HostRoot string `json:"-"`
	// CommandHostRoot holds the session hosts of managed commands, which
	// outlive the Runner too. Empty selects <State>/command-hosts.
	CommandHostRoot string                   `json:"-"`
	Worker          string                   `json:"worker"`
	CodexBinary     string                   `json:"codexBinary,omitempty"`
	ClaudeBinary    string                   `json:"claudeBinary,omitempty"`
	Profiles        []accounts.Profile       `json:"profiles"`
	Gateways        []Gateway                `json:"gateways,omitempty"`
	Defaults        accounts.Options         `json:"defaults"`
	DefaultGateways map[string]string        `json:"defaultGateways,omitempty"`
	EventBudget     int64                    `json:"eventBudget,omitempty"`
	CommandBudget   int64                    `json:"commandBudget,omitempty"`
	WorktreeRoot    string                   `json:"worktreeRoot,omitempty"`
	Repositories    []collections.Repository `json:"repositories,omitempty"`
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
}
type Bench struct {
	RepositoryCatalog   *repositorycatalog.Cache
	Telemetry           telemetry.Store
	Meter               *telemetry.Gateway
	Prices              telemetry.Pricer
	Preferences         *preferences.Store
	GitHub              *githubconnection.Manager
	GitHubAPI           github.API
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
	Attachments         *attachments.Store
	Collections         *collections.Manager
	ctx                 context.Context
	cancel              context.CancelFunc
	items               map[string]*Execution
	opening             int
	closed              bool
	pending             map[string]bool
	starts              sync.WaitGroup
	closeOnce           sync.Once
	closeErr            error
	// Adapters is an injection boundary for deterministic conformance tests.
	Adapters map[string]agent.Adapter
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
	if !filepath.IsAbs(cfg.HostRoot) || !filepath.IsAbs(cfg.CommandHostRoot) {
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
	// Commands a previous Runner left running are re-attached here, before any
	// execution can start or inspect one.
	manager, err := commands.NewHosted(filepath.Join(cfg.State, "commands"), cfg.CommandBudget, cfg.MaxLiveCommands, cfg.CommandHostRoot)
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
	b := &Bench{History: history, archives: map[string]*Execution{}, Config: cfg, profiles: append([]accounts.Profile(nil), cfg.Profiles...), Accounts: accountState, Commands: manager, Attachments: files, Collections: groups, ctx: ctx, cancel: cancel, pending: map[string]bool{}, enrollments: map[string]*Enrollment{}, items: map[string]*Execution{}, Adapters: map[string]agent.Adapter{"codex": codex.Adapter{Binary: cfg.CodexBinary}, "claude": claude.Adapter{Worker: cfg.Worker, Binary: cfg.ClaudeBinary}}}
	privateStore, err := githubconnection.NewFileStore(filepath.Join(cfg.State, "connections"))
	if err == nil {
		b.GitHubAPI = github.New()
		b.GitHub, err = githubconnection.New(privateStore, b.GitHubAPI)
	}
	if err == nil && cfg.GitHubApp != nil {
		_, err = b.GitHub.Configure(*cfg.GitHubApp)
	}
	if err == nil && groups != nil {
		b.RepositoryImporter, err = repositories.New(filepath.Join(cfg.State, "repositories"), groups, repositories.Git{})
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
	b.Prices = b.Preferences
	b.Telemetry, err = telemetry.Open(filepath.Join(cfg.State, "telemetry.db"))
	if err == nil {
		b.Meter, err = telemetry.StartGateway(b.Telemetry, b.Prices, cfg.MeterUpstreams)
	}
	if err != nil {
		_ = b.Detach()
		return nil, err
	}
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
	return b.open(profile, cwd, resume, options, gatewayID, workID, previous, "", "coding", "", false)
}

func (b *Bench) OpenCollection(profile, collectionID, resume string, options accounts.Options, gatewayID, previous string) (*Execution, error) {
	if collectionID == "" {
		return nil, errors.New("collection id required")
	}
	return b.open(profile, "", resume, options, gatewayID, "", previous, collectionID, "coding", "", false)
}

// OpenEmptyWorkspace starts a coding session in a new Runner-owned directory.
// Closing the session retains its files; no Git repository is initialized.
func (b *Bench) OpenEmptyWorkspace(profile string, options accounts.Options, gatewayID string) (*Execution, error) {
	return b.open(profile, "", "", options, gatewayID, "", "", "", "coding", "", true)
}

// OpenAssistant opens a bounded application conversation without a Collection
// or repository lease. Coding permissions and provider configuration are not saved.
func (b *Bench) OpenAssistant(profile, purpose, contextText string, options accounts.Options, gatewayID string, workspace ...string) (*Execution, error) {
	if !agent.IsAssistant(purpose) || len(contextText) > 128<<10 {
		return nil, errors.New("assistant purpose must be help, planning, or summary; context max 128 KiB")
	}
	cwd := filepath.Join(b.Config.State, "assistants")
	if err := os.MkdirAll(cwd, 0700); err != nil {
		return nil, err
	}
	options.Permission = "read-only"
	options.Workflow = "execute"
	contextText = assistant.Context + "\nYou are the Lazybox application assistant for " + purpose + ". Answer from the supplied context. Produce advice or a proposal; do not execute commands, edit files, or apply configuration. Supplied context follows:\n" + contextText
	collectionID := ""
	if len(workspace) > 0 {
		collectionID = workspace[0]
	}
	return b.open(profile, cwd, "", options, gatewayID, "", "", collectionID, purpose, contextText, false)
}

func (b *Bench) open(profile, cwd, resume string, options accounts.Options, gatewayID, workID, previous, collectionID, purpose, suppliedContext string, emptyWorkspace bool) (*Execution, error) {
	if err := b.ReloadProfiles(); err != nil {
		return nil, err
	}
	id := commands.ID()
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
	if parent == nil {
		prefs := b.Preferences.Snapshot()
		extra := prefs.ClaudeContext
		if selected.Provider == "codex" {
			extra = prefs.CodexContext
		}
		if options.Instructions == "" {
			options.Instructions = b.Config.Defaults.Instructions
		}
		contextText += "\n" + prefs.Context + "\n" + extra + "\n" + options.Instructions
		if options.Workflow == "" {
			options.Workflow = b.Config.Defaults.Workflow
		}
		if options.Permission == "" && prefs.Permission != "" {
			options.Permission = prefs.Permission
			if options.Workflow == "plan" {
				options.Permission = "read-only"
			}
		}
		if options.RotationProfiles == nil {
			options.RotationProfiles = b.Config.Defaults.RotationProfiles
		}
		options.Gateway = nil
		if options.Model == "" {
			options.Model = b.Config.Defaults.Model
		}
		if options.Permission == "" {
			options.Permission = b.Config.Defaults.Permission
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
	if err = options.Validate(); err != nil {
		return nil, err
	}
	env, err := accounts.Resolve(selected, os.Environ())
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
	env = append(env, "LAZYBOX_V2_ENDPOINT="+b.Config.Endpoint)
	env = append(env, "LAZYBOX_V2_SESSION_ID="+id)
	if collectionID != "" {
		env = append(env, "LAZYBOX_V2_COLLECTION_ID="+collectionID)
	}
	if workID == "" {
		workID = id
	}
	spool, err := journal.Open(filepath.Join(b.Config.State, "sessions", id+".jsonl"), b.Config.EventBudget)
	if err != nil {
		return nil, err
	}
	e := &Execution{ID: id, WorkID: workID, Previous: previous, Cwd: cwd, CollectionID: collectionID, Purpose: purpose, Profile: selected, Options: options, Env: env, spool: spool}
	conversationID := id
	if parent != nil {
		conversationID = parent.ConversationID
	}
	adapter := b.sessionAdapter(e, selected, options, conversationID)
	record := sessionstore.Record{ID: id, ConversationID: conversationID, Previous: previous, WorkID: workID, CollectionID: collectionID, Cwd: cwd, Purpose: purpose, Context: contextText, ProfileID: profile, Provider: selected.Provider, ProfileBinding: profileBinding(selected), Options: options, GatewayID: b.GatewayID(e), Title: filepath.Base(cwd), CreatedAt: time.Now().UTC(), Snapshot: engine.Snapshot{ID: id, Profile: profile, Purpose: purpose, Reference: resume}}
	record.ProfileBindings = map[string]string{}
	for _, p := range b.Profiles() {
		record.ProfileBindings[p.ID] = profileBinding(p)
	}
	if parent != nil {
		record.Title = parent.Title
		if parent.Snapshot.Title != "" {
			record.Title = parent.Snapshot.Title
		}
		record.Snapshot.Capabilities = parent.Snapshot.Capabilities
		record.ProviderSettings = parent.ProviderSettings
	}
	e.Saved = record
	if err = b.History.Create(b.ctx, record); err != nil {
		spool.Dispose()
		return nil, err
	}
	open := b.sessionOpen(e, contextText, resume)
	if parent != nil && profile == parent.Snapshot.Profile {
		open.ProviderSettings = parent.ProviderSettings
	}
	eng, err := engine.NewDurable(b.ctx, adapter, open, spool, func(snapshot engine.Snapshot, settings json.RawMessage) error {
		return b.History.SaveState(context.Background(), id, snapshot, settings)
	}, func(a engine.Attachment) error { return b.saveAttachment(id, a) })
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
	return e, nil
}

func (b *Bench) Start(e *Execution, title string, argv []string) (*commands.Command, error) {
	return b.start(e, title, argv, nil)
}
func (b *Bench) start(e *Execution, title string, argv []string, pinnedEnv []string) (*commands.Command, error) {
	e.activity.Lock()
	defer e.activity.Unlock()
	if agent.IsAssistant(e.Purpose) || e.Options.Permission == "read-only" || e.Options.Workflow == "plan" {
		return nil, errors.New("managed command start is disabled for read-only executions")
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
					env, err = accounts.Resolve(p, os.Environ())
					if err != nil {
						return nil, err
					}
					env = append(env, "LAZYBOX_V2_ENDPOINT="+b.Config.Endpoint, "LAZYBOX_V2_SESSION_ID="+e.ID)
					if e.CollectionID != "" {
						env = append(env, "LAZYBOX_V2_COLLECTION_ID="+e.CollectionID)
					}
				}
			}
		}
	}
	if pinnedEnv != nil {
		env = pinnedEnv
	}
	return b.Commands.Start(commands.Spec{SessionID: e.ID, Title: title, Cwd: e.Cwd, Argv: argv, Env: env})
}

func (b *Bench) RegisterPipe(e *Execution, title string) (*commands.Command, error) {
	e.activity.Lock()
	defer e.activity.Unlock()
	if e.Engine.Snapshot().Closed {
		return nil, errors.New("execution closed")
	}
	return b.Commands.Register(commands.Spec{SessionID: e.ID, Title: title, Cwd: e.Cwd})
}

func (b *Bench) toolWithEnv(ctx context.Context, e *Execution, name string, args json.RawMessage, pinnedEnv []string) (string, error) {
	if err := ctx.Err(); err != nil {
		return "", err
	}
	if name != "lazybox_command" {
		return "", agent.ErrUnsupported
	}
	var p struct {
		Action string   `json:"action"`
		Argv   []string `json:"argv"`
		ID     string   `json:"id"`
		Offset uint64   `json:"offset"`
	}
	if err := json.Unmarshal(args, &p); err != nil {
		return "", err
	}
	var value any
	if p.Action == "start" {
		c, err := b.start(e, "agent command", p.Argv, pinnedEnv)
		if err != nil {
			return "", err
		}
		value = c.Status()
	} else {
		c, err := b.Commands.Get(p.ID)
		if err != nil {
			return "", err
		}
		if c.Status().Spec.SessionID != e.ID && c.Status().Spec.SessionID != e.Previous {
			return "", errors.New("command belongs to another execution")
		}
		switch p.Action {
		case "read":
			data, status, err := c.Read(ctx, p.Offset, 32<<10)
			if err != nil {
				return "", err
			}
			value = map[string]any{"output": string(data), "nextOffset": p.Offset + uint64(len(data)), "status": status}
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
		if b.GitHub != nil {
			b.GitHub.Close()
		}
		b.starts.Wait()
		for _, e := range b.List() {
			// A no-op for a detached engine: its agent is the next Runner's.
			b.closeErr = errors.Join(b.closeErr, e.Engine.Close())
		}
		if detach {
			b.closeErr = errors.Join(b.closeErr, b.Commands.Detach())
		} else {
			b.closeErr = errors.Join(b.closeErr, b.Commands.Close())
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
	enrolled, err := accounts.LoadProfiles(b.Config.SubscriptionState)
	if err != nil {
		return err
	}
	b.profileMu.Lock()
	defer b.profileMu.Unlock()
	next := append([]accounts.Profile(nil), b.profiles...)
	byID := map[string]accounts.Profile{}
	for _, p := range b.profiles {
		byID[p.ID] = p
	}
	for _, p := range enrolled {
		if old, exists := byID[p.ID]; exists {
			if old.Provider != p.Provider || old.Home != p.Home || old.TokenFile != p.TokenFile {
				return errors.New("enrolled profile conflicts with configured identity")
			}
			continue
		}
		byID[p.ID] = p
		next = append(next, p)
	}
	b.profiles = next
	return nil
}
