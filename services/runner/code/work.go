package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"syscall"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/remote"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/restart"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/transport"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/adapters"
	"github.com/codefly-dev/core/standards"
	codefly "github.com/codefly-dev/sdk-go"
)

func init() { WithConfigure(configureRunner) }

// configureRunner adds execution behavior through Codefly's generated seam;
// the scaffold owns listeners, network resolution, health, and shutdown.
func configureRunner(ctx context.Context, config *adapters.Configuration) (Clean, error) {
	return configureRunnerAt(ctx, config, func() (string, error) {
		instance, err := codefly.For(ctx).API(standards.CONNECT).ResolveNetworkInstance()
		if err != nil {
			return "", err
		}
		return instance.Address, nil
	})
}

// configureRunnerAt is the Runner behind either launch: Codefly's generated
// main (configureRunner) or an installed release's standalone serve mode
// (standalone.go). address resolves the Connect URL clients reach it at.
func configureRunnerAt(ctx context.Context, config *adapters.Configuration, address func() (string, error)) (Clean, error) {
	if err := requireLoopbackListeners(ctx, config); err != nil {
		return nil, err
	}
	cfg := runner.Config{}
	// remote.httpsPort: the tailnet HTTPS port a phone reaches the Runner on (REMOTE.md).
	var remoteConfig struct {
		Remote struct {
			HTTPSPort uint32 `json:"httpsPort"`
		} `json:"remote"`
	}
	if path := os.Getenv("LAZYBOX_V2_CONFIG"); path != "" {
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		if err = json.Unmarshal(data, &cfg); err != nil {
			return nil, err
		}
		if err = json.Unmarshal(data, &remoteConfig); err != nil {
			return nil, err
		}
	}
	if shared := os.Getenv("LAZYBOX_V2_SUBSCRIPTION_STATE"); shared != "" {
		cfg.SubscriptionState = shared
	}
	if command := os.Getenv("LAZYBOX_V2_SNIPPET_COMMAND"); command != "" && cfg.SnippetCommand == "" {
		cfg.SnippetCommand = command
	}
	if cfg.SnippetDirectory == "" {
		cfg.SnippetDirectory = os.Getenv("LAZYBOX_V2_SNIPPET_DIRECTORY")
	}
	state := os.Getenv("LAZYBOX_V2_STATE")
	if state == "" {
		var err error
		state, err = os.MkdirTemp("", "lazybox-v2-bench-")
		if err != nil {
			return nil, err
		}
	}
	var err error
	cfg.State, err = filepath.Abs(state)
	if err != nil {
		return nil, err
	}
	if err = os.MkdirAll(cfg.State, 0700); err != nil {
		return nil, err
	}
	lock, err := os.OpenFile(filepath.Join(cfg.State, "runner.lock"), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, err
	}
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		lock.Close()
		return nil, errors.New("test state already owned by another Runner")
	}
	if worker := os.Getenv("LAZYBOX_V2_WORKER"); worker != "" {
		cfg.Worker = worker
	}
	// An installed release names the Node and Claude Code it ships (standalone.go);
	// a choice in the Runner configuration still wins.
	if node := os.Getenv("LAZYBOX_V2_NODE"); node != "" && cfg.Node == "" {
		cfg.Node = node
	}
	if claude := os.Getenv("LAZYBOX_V2_CLAUDE_BINARY"); claude != "" && cfg.ClaudeBinary == "" {
		cfg.ClaudeBinary = claude
	}
	// The agent browser service (BROWSER.md); unset, agents get no browser.
	if worker := os.Getenv("LAZYBOX_V2_BROWSER_WORKER"); worker != "" {
		cfg.BrowserWorker = worker
	}
	// The documentation the help assistant reads: the installed release's, or
	// the Dev and Working source's docs folder (scripts/runner-command.mjs).
	if docs := os.Getenv("LAZYBOX_V2_DOCS_DIR"); docs != "" && cfg.DocsDirectory == "" {
		cfg.DocsDirectory = docs
	}
	if dir := os.Getenv("LAZYBOX_V2_CLI_DIR"); dir != "" {
		// Only brief agents about a CLI they can actually run (older releases have none).
		if info, err := os.Stat(filepath.Join(dir, "lazybox")); err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0 {
			cfg.CLIDir = dir
		}
	}
	// The ACP agents installed on this computer are offered (ACP.md).
	cfg.ACPLookPath = exec.LookPath
	if root := os.Getenv("LAZYBOX_V2_WORKTREE_ROOT"); root != "" {
		cfg.WorktreeRoot = root
	}
	// The person's actions and hooks file (POWER.md); the launchers pass
	// ~/.config/lazybox/actions.yaml. Unset, the Runner reads <state>/actions.yaml.
	if file := os.Getenv("LAZYBOX_V2_ACTIONS_FILE"); file != "" && cfg.ActionsFile == "" {
		cfg.ActionsFile = file
	}
	if cfg.ActionsFile != "" && !filepath.IsAbs(cfg.ActionsFile) {
		return nil, errors.New("actionsFile must be an absolute path")
	}
	// The owner's endpoint file. Agents get their own (runner.Bench.AgentEndpoint).
	endpoint := filepath.Join(cfg.State, "endpoint.json")
	cfg.Address, err = address()
	if err != nil {
		lock.Close()
		return nil, err
	}
	bench, err := runner.New(cfg)
	if err != nil {
		lock.Close()
		return nil, err
	}
	runnerID, err := bench.History.Identity(ctx)
	if err != nil {
		_ = bench.Detach() // A failed start leaves adopted commands running.
		lock.Close()
		return nil, err
	}
	token := commands.ID() + commands.ID()
	data, err := json.Marshal(map[string]string{"url": cfg.Address, "token": token, "runnerId": runnerID})
	if err == nil {
		var file *os.File
		file, err = os.CreateTemp(cfg.State, ".endpoint-")
		if err == nil {
			defer os.Remove(file.Name())
			_, err = file.Write(data)
			if err == nil {
				err = file.Sync()
			}
			err = errors.Join(err, file.Close())
			if err == nil {
				err = os.Rename(file.Name(), endpoint)
			}
		}
	}
	if err != nil {
		_ = bench.Detach() // A failed start leaves adopted commands running.
		lock.Close()
		return nil, err
	}
	// A persistent environment re-attaches the agents its previous Runner left
	// running and leaves them running when this one stops. Ephemeral test
	// state keeps the old contract: agents stop with their Runner.
	keepAgents := os.Getenv("LAZYBOX_V2_KEEP_AGENTS") == "1"
	if keepAgents {
		if err = bench.Adopt(ctx); err != nil {
			fmt.Fprintf(os.Stderr, "V2 Runner: some live executions were not re-attached: %v\n", err)
		}
	} else if err = bench.StopUnclaimedHosts(ctx); err != nil {
		fmt.Fprintf(os.Stderr, "V2 Runner: some agents of a previous Runner were not stopped: %v\n", err)
	}
	shutdown := bench.Close
	if keepAgents {
		shutdown = bench.Detach
	}
	service := &transport.Service{Bench: bench}
	// A launcher that will start this Runner again names where it reads the
	// request; RestartRunner then stops through the shutdown above.
	supervisor, err := restart.FromEnvironment(keepAgents)
	if err != nil {
		fmt.Fprintf(os.Stderr, "V2 Runner: restart from the app is unavailable: %v\n", err)
	} else if supervisor != nil {
		service.Restart = supervisor
	}
	config.Service = service
	config.GRPCServerOptions = append(config.GRPCServerOptions, transport.LocalAuthority(token, bench)...)
	config.MaxMessageBytes = 36 << 20
	stopRemote, err := startRemote(ctx, cfg.State, service, config.MaxMessageBytes, remoteConfig.Remote.HTTPSPort)
	if err != nil {
		_ = shutdown()
		lock.Close()
		return nil, err
	}
	fmt.Printf("V2 Runner endpoint file: %s\n", endpoint)
	// Stop or detach the engines before Codefly drains streams on shutdown. Merely
	// closing a viewer cancels its subscription and does not reach this service lifetime.
	go func() { <-ctx.Done(); stopRemote(); _ = shutdown() }()
	return func() { stopRemote(); _ = shutdown(); _ = lock.Close() }, nil
}

// startRemote opens the device gateway (REMOTE.md): paired phones' only way
// in, on loopback, with device credentials alone. A gateway that cannot bind
// leaves the Runner serving this machine and says why in Settings → Devices.
func startRemote(ctx context.Context, state string, service *transport.Service, maxMessageBytes int, httpsPort uint32) (func(), error) {
	manager, err := devices.Open(state)
	if err != nil {
		return nil, err
	}
	handler, stopRPC, err := transport.DeviceHandler(service, manager, maxMessageBytes)
	if err != nil {
		return nil, err
	}
	gateway, err := remote.New(state, manager, handler)
	if err != nil {
		stopRPC()
		return nil, err
	}
	gateway.WebDir = os.Getenv("LAZYBOX_V2_WEB_DIR")
	if httpsPort != 0 {
		gateway.HTTPSPort = httpsPort
	}
	if err = gateway.Listen(ctx); err != nil {
		fmt.Fprintf(os.Stderr, "V2 Runner: the device gateway is unavailable: %v\n", err)
	} else {
		fmt.Printf("V2 Runner device gateway: %s\n", gateway.LocalURL())
	}
	service.Devices, service.Remote = manager, gateway
	// Push notifications to paired phones: a VAPID key of this Runner's own,
	// and a notifier that watches the conversations as the desktop inbox does.
	stopPush := func() {}
	if keys, err := webpush.LoadOrCreateKey(filepath.Join(state, "vapid.key")); err != nil {
		fmt.Fprintf(os.Stderr, "V2 Runner: phone notifications are unavailable: %v\n", err)
	} else {
		notifier := &remote.Notifier{Devices: manager, Sender: webpush.NewSender(keys), Sessions: service.Sessions, Keys: keys}
		notifier.Start(ctx)
		service.Push, stopPush = notifier, notifier.Stop
		// A tracked pull request's transitions, under the same settings.
		service.Bench.SubscribeItemEvents(func(ev items.Event) {
			notifier.PullRequest(remote.PullRequestNotice{Event: ev.Name, Repository: ev.Repository, Number: ev.Number, Title: ev.Title, Reason: ev.Reason})
		})
	}
	var once sync.Once
	return func() { once.Do(func() { stopPush(); _ = gateway.Close(); stopRPC() }) }, nil
}
