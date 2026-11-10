package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"syscall"

	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/v2/internal/transport"
	"github.com/AntoineToussaint/lazybox/v2/pkg/adapters"
	"github.com/codefly-dev/core/standards"
	codefly "github.com/codefly-dev/sdk-go"
)

func init() { WithConfigure(configureRunner) }

// configureRunner adds execution behavior through Codefly's generated seam;
// the scaffold owns listeners, network resolution, health, and shutdown.
func configureRunner(ctx context.Context, config *adapters.Configuration) (Clean, error) {
	cfg := runner.Config{}
	if path := os.Getenv("LAZYBOX_V2_CONFIG"); path != "" {
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		if err = json.Unmarshal(data, &cfg); err != nil {
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
	if root := os.Getenv("LAZYBOX_V2_WORKTREE_ROOT"); root != "" {
		cfg.WorktreeRoot = root
	}
	cfg.Endpoint = filepath.Join(cfg.State, "endpoint.json")
	instance, err := codefly.For(ctx).API(standards.CONNECT).ResolveNetworkInstance()
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
		bench.Close()
		lock.Close()
		return nil, err
	}
	token := commands.ID() + commands.ID()
	data, err := json.Marshal(map[string]string{"url": instance.Address, "token": token, "runnerId": runnerID})
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
				err = os.Rename(file.Name(), cfg.Endpoint)
			}
		}
	}
	if err != nil {
		bench.Close()
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
	}
	shutdown := bench.Close
	if keepAgents {
		shutdown = bench.Detach
	}
	config.Service = &transport.Service{Bench: bench}
	config.GRPCServerOptions = append(config.GRPCServerOptions, transport.LocalAuthority(token)...)
	config.MaxMessageBytes = 36 << 20
	fmt.Printf("V2 Runner endpoint file: %s\n", cfg.Endpoint)
	// Stop or detach the engines before Codefly drains streams on shutdown. Merely
	// closing a viewer cancels its subscription and does not reach this service lifetime.
	go func() { <-ctx.Done(); _ = shutdown() }()
	return func() { _ = shutdown(); _ = lock.Close() }, nil
}
