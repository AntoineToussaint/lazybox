package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"syscall"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/restart"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/adapters"
)

// An installed release runs the Runner without Codefly: `lazybox-runner
// serve`, started by its service manager (internal/installation: a launchd
// job on macOS, a systemd user unit on Linux, or `lazybox-runner supervise`
// where there is no systemd). Codefly's
// generated main stays the launch path for Dev, Working and `make run`;
// both reach the same configureRunnerAt, with the same state, loopback-only
// listeners, endpoint.json and agent/terminal credentials.
//
// The mode is chosen before the generated main runs, the way session hosts
// and processguard re-execute this binary, so no generated file changes.
func init() {
	if len(os.Args) < 2 {
		return
	}
	switch os.Args[1] {
	case "serve":
		if err := serve(os.Args[2:]); err != nil {
			fmt.Fprintf(os.Stderr, "Lazybox Runner: %v\n", err)
			os.Exit(1)
		}
		os.Exit(0)
	case installation.SuperviseArgument:
		// `lazybox-runner supervise <service.json> <supervisor.json>`: the
		// service manager where there is none (installation.Supervisor).
		if len(os.Args) != 4 {
			fmt.Fprintln(os.Stderr, "usage: lazybox-runner supervise <service.json> <supervisor.json>")
			os.Exit(2)
		}
		if err := installation.Supervise(os.Args[2], os.Args[3]); err != nil {
			fmt.Fprintf(os.Stderr, "Lazybox supervisor: %v\n", err)
			os.Exit(1)
		}
		os.Exit(0)
	case "--version", "version":
		fmt.Println("lazybox-runner " + version.String())
		os.Exit(0)
	}
}

// standaloneOptions are the serve flags. Every path defaults to the installed
// layout (installation.UserPaths) and to the runtime this binary ships in
// (installation.Current); a flag or the matching LAZYBOX_V2_* variable wins.
type standaloneOptions struct {
	state, config, worktrees, actions, restartRequest string
	worker, node, web, cliDir, claude, docs           string
	// browserWorker is the agent browser service (BROWSER.md); the installer
	// does not ship it yet, so an installed Runner has no agent browser unless
	// this names one.
	browserWorker          string
	supervised, keepAgents bool
}

func parseStandalone(args []string, getenv func(string) string, runtime *installation.Layout) (standaloneOptions, error) {
	paths, err := installation.UserPaths(getenv)
	if err != nil {
		return standaloneOptions{}, err
	}
	pick := func(variable, fallback string) string {
		if v := getenv(variable); v != "" {
			return v
		}
		return fallback
	}
	o := standaloneOptions{}
	var worker, node, web, cliDir, claude, docs string
	if runtime != nil {
		worker, node, web, cliDir = runtime.Worker(), runtime.Node(), runtime.Web(), runtime.Bin()
		if _, err := os.Stat(runtime.ClaudeBinary()); err == nil {
			claude = runtime.ClaudeBinary()
		}
		// Older releases shipped no documentation for the help assistant.
		if info, err := os.Stat(runtime.Docs()); err == nil && info.IsDir() {
			docs = runtime.Docs()
		}
	}
	flags := flag.NewFlagSet("serve", flag.ContinueOnError)
	flags.StringVar(&o.state, "state", pick("LAZYBOX_V2_STATE", paths.State), "Runner state directory")
	flags.StringVar(&o.config, "config", pick("LAZYBOX_V2_CONFIG", paths.Config), "Runner configuration (runner.config.json); created when missing")
	flags.StringVar(&o.worktrees, "worktrees", pick("LAZYBOX_V2_WORKTREE_ROOT", paths.Worktrees), "where workspaces' worktrees are created")
	flags.StringVar(&o.actions, "actions", pick("LAZYBOX_V2_ACTIONS_FILE", paths.ActionsFile()), "the person's actions and hooks file")
	flags.StringVar(&o.restartRequest, "restart-request", paths.RestartRequest, "where Restart Runner records its request (with --supervised)")
	flags.StringVar(&o.worker, "worker", pick("LAZYBOX_V2_WORKER", worker), "Claude SDK worker (dist/main.js)")
	flags.StringVar(&o.node, "node", pick("LAZYBOX_V2_NODE", node), "Node that runs the worker")
	flags.StringVar(&o.browserWorker, "browser-worker", getenv("LAZYBOX_V2_BROWSER_WORKER"), "agent browser service (workers/browser/dist/main.js); empty: agents get no browser")
	flags.StringVar(&o.web, "web", pick("LAZYBOX_V2_WEB_DIR", web), "phone app the device gateway serves")
	flags.StringVar(&o.cliDir, "cli-dir", pick("LAZYBOX_V2_CLI_DIR", cliDir), "directory of the lazybox CLI agents and hooks run")
	flags.StringVar(&o.claude, "claude", pick("LAZYBOX_V2_CLAUDE_BINARY", claude), "Claude Code executable (default: the one the SDK ships)")
	flags.StringVar(&o.docs, "docs", pick("LAZYBOX_V2_DOCS_DIR", docs), "Lazybox documentation the help assistant reads (default: the release's)")
	flags.BoolVar(&o.supervised, "supervised", false, "a service manager (launchd, systemd, the supervisor) starts this Runner again whenever it exits: offer Restart Runner")
	flags.BoolVar(&o.keepAgents, "keep-agents", true, "leave agents, commands and terminals running across a Runner restart")
	if err := flags.Parse(args); err != nil {
		return o, err
	}
	if flags.NArg() > 0 {
		return o, fmt.Errorf("unexpected argument %q", flags.Arg(0))
	}
	for name, path := range map[string]string{"--state": o.state, "--config": o.config, "--worktrees": o.worktrees, "--actions": o.actions, "--restart-request": o.restartRequest} {
		if !filepath.IsAbs(path) {
			return o, fmt.Errorf("%s must be an absolute path", name)
		}
	}
	if o.worker == "" {
		return o, errors.New("no Claude SDK worker: run the Runner from an installed Lazybox, or pass --worker")
	}
	return o, nil
}

// environment is what configureRunnerAt reads, the same variables the Dev and
// Working launchers set through Codefly (scripts/runner-command.mjs).
func (o standaloneOptions) environment() map[string]string {
	env := map[string]string{
		"LAZYBOX_V2_STATE":          o.state,
		"LAZYBOX_V2_CONFIG":         o.config,
		"LAZYBOX_V2_WORKTREE_ROOT":  o.worktrees,
		"LAZYBOX_V2_ACTIONS_FILE":   o.actions,
		"LAZYBOX_V2_WORKER":         o.worker,
		"LAZYBOX_V2_NODE":           o.node,
		"LAZYBOX_V2_BROWSER_WORKER": o.browserWorker,
		"LAZYBOX_V2_WEB_DIR":        o.web,
		"LAZYBOX_V2_CLI_DIR":        o.cliDir,
		"LAZYBOX_V2_CLAUDE_BINARY":  o.claude,
		"LAZYBOX_V2_DOCS_DIR":       o.docs,
	}
	if o.keepAgents {
		env["LAZYBOX_V2_KEEP_AGENTS"] = "1"
	}
	if o.supervised {
		env[restart.Environment] = o.restartRequest
	}
	return env
}

func serve(args []string) error {
	var runtime *installation.Layout
	if l, err := installation.Current(); err == nil {
		runtime = &l
	}
	o, err := parseStandalone(args, os.Getenv, runtime)
	if err != nil {
		return err
	}
	for _, dir := range []string{o.state, filepath.Dir(o.config)} {
		if err = os.MkdirAll(dir, 0o700); err != nil {
			return err
		}
	}
	// The private configuration a person edits (RUNNING.md); never replaced.
	if f, err := os.OpenFile(o.config, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600); err == nil {
		_, err = f.WriteString("{\n  \"profiles\": []\n}\n")
		if err = errors.Join(err, f.Close()); err != nil {
			return err
		}
	} else if !errors.Is(err, os.ErrExist) {
		return err
	}
	// The service manager starts the next Runner whatever the reason for an
	// exit, so a request left by an earlier one means nothing now.
	if o.supervised {
		_ = os.Remove(o.restartRequest)
	}
	for k, v := range o.environment() {
		if v == "" {
			continue
		}
		if err = os.Setenv(k, v); err != nil {
			return err
		}
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	port, err := loopbackPort()
	if err != nil {
		return err
	}
	// Connect only: every client (desktop, CLI, browser, phone gateway)
	// speaks Connect, and the transcoder dispatches to the in-process gRPC
	// server, so a second listener would only be another port to guard.
	config := &adapters.Configuration{EndpointConnectPort: &port, EndpointConnectHost: "localhost"}
	address := "http://localhost:" + strconv.Itoa(int(port))
	clean, err := configureRunnerAt(ctx, config, func() (string, error) { return address, nil })
	if err != nil {
		return err
	}
	server, err := adapters.NewServer(config)
	if err != nil {
		clean()
		return err
	}
	fmt.Printf("Lazybox Runner %s serving at %s (state %s)\n", version.String(), address, o.state)
	failed := make(chan error, 1)
	go func() { failed <- server.Start(ctx) }()
	select {
	case <-ctx.Done():
		err = nil
	case err = <-failed:
		if err == nil {
			err = errors.New("the Connect listener stopped")
		}
	}
	server.Stop()
	clean()
	return err
}

// loopbackPort is a port free on localhost now. The generated listeners take
// a port number, not a bound listener, so this is the same allocation Codefly's
// --temporary-ports makes for Dev and Working: a port another process takes
// in between makes the listener fail, the Runner exit, and its service
// manager start it again on another one. (Accepting a bound net.Listener belongs in the
// generated adapters, which are Codefly's go-grpc agent's to change.)
func loopbackPort() (uint16, error) {
	l, err := net.Listen("tcp", "localhost:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return uint16(l.Addr().(*net.TCPAddr).Port), nil
}
