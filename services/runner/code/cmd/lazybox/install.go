package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// The installed release's own verbs (INSTALL.md): the Runner service,
// the CLI on PATH, a health check and a clean removal. They act on the
// release this `lazybox` belongs to; a Dev or Working build says so.

// setup is how this process finds its release and the system around it;
// tests replace it so nothing touches the real launchd or home directory.
type setup struct {
	installer func() (installation.Installer, error)
	// ready waits for the service's Runner to answer, given the bearer the
	// previous one held; it returns that Runner's version.
	ready func(ctx context.Context, endpoint, previousToken string) (string, error)
}

func defaultSetup(getenv func(string) string) setup {
	return setup{
		installer: func() (installation.Installer, error) {
			runtime, err := installation.Current()
			if errors.Is(err, installation.ErrNotInstalled) {
				return installation.Installer{}, fail(exitUnavailable, "this lazybox is a development build, not part of an installed Lazybox; Dev and Working run their Runner with `make dev` or `make working`")
			}
			if err != nil {
				return installation.Installer{}, err
			}
			paths, err := installation.UserPaths(getenv)
			if err != nil {
				return installation.Installer{}, err
			}
			return installation.Installer{Paths: paths, Layout: runtime, Launchd: installation.UserLaunchd(),
				Shell: func(ctx context.Context) installation.ShellEnvironment {
					return installation.ReadShellEnvironment(ctx, getenv)
				}}, nil
		},
		ready: waitForRunner,
	}
}

func (c *cli) setup() setup {
	if c.install.installer == nil {
		c.install = defaultSetup(c.getenv)
	}
	return c.install
}

// ---- version ----

func (c *cli) version(args []string) error {
	f := c.flags("version")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	info := map[string]string{"version": version.String()}
	if runtime, err := installation.Current(); err == nil {
		info["runtime"] = runtime.Root
		if app := runtime.AppBundle(); app != "" {
			info["app"] = app
		}
	}
	if c.json {
		return c.printJSON(info)
	}
	c.out("%s\n", version.Line())
	if app := info["app"]; app != "" {
		c.out("%s\n", c.paint(dim, app))
	}
	return nil
}

// ---- service ----

type serviceReport struct {
	installation.Status
	Reachable      bool   `json:"reachable"`
	URL            string `json:"url,omitempty"`
	RunnerVersion  string `json:"runnerVersion,omitempty"`
	ReleaseVersion string `json:"releaseVersion,omitempty"`
	Problem        string `json:"problem,omitempty"`
	// Unstable is why this copy of the app cannot back a service (run from a disk image).
	Unstable string `json:"unstable,omitempty"`
}

func (c *cli) service(args []string) error {
	f := c.flags("service")
	noStart := f.Bool("no-start", false, "install: write the service but do not start it")
	follow := f.Bool("follow", false, "logs: keep printing new lines")
	f.BoolVar(follow, "f", false, "logs: keep printing new lines")
	lines := f.Int("lines", 80, "logs: how many lines to print")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	sub := "status"
	if len(positional) > 0 {
		sub = positional[0]
	}
	in, err := c.setup().installer()
	if err != nil {
		return err
	}
	ctx := c.ctx
	switch sub {
	case "install", "update":
		return c.serviceInstall(ctx, in, !*noStart)
	case "uninstall":
		kept := c.liveProcesses(in.Paths.Endpoint())
		removed, err := in.Uninstall(ctx)
		if err != nil {
			return err
		}
		if c.json {
			return c.printJSON(map[string]any{"removed": removed, "keptRunning": kept})
		}
		c.out("%s Removed the Runner service. Your conversations, subscriptions and settings are kept in %s.\n", c.paint(green, "✓"), in.Paths.Support)
		if kept.total() > 0 {
			c.out("  Still running: %s. A Runner re-attaches them when you install the service again; each stops by itself after a day without one.\n", kept)
		}
		return nil
	case "start":
		previous := endpointToken(in.Paths.Endpoint())
		if err = in.Start(ctx); err != nil {
			return err
		}
		return c.reportReady(ctx, in, previous, "Started the Runner")
	case "stop":
		if err = in.Stop(ctx); err != nil {
			return err
		}
		if c.json {
			return c.printJSON(map[string]bool{"stopped": true})
		}
		c.out("%s Stopped the Runner. Agents, commands and terminals keep running; the next Runner re-attaches them. It starts again at login, or with `lazybox service start`.\n", c.paint(green, "✓"))
		return nil
	case "restart":
		previous := endpointToken(in.Paths.Endpoint())
		if err = in.Restart(ctx); err != nil {
			return err
		}
		return c.reportReady(ctx, in, previous, "Restarted the Runner")
	case "status":
		r := c.serviceStatus(ctx, in)
		if c.json {
			return c.printJSON(r)
		}
		c.printServiceStatus(r)
		if !r.Installed || !r.Reachable {
			return exitError{exitUnavailable, errors.New(r.Problem)}
		}
		return nil
	case "logs":
		return c.serviceLogs(in.Paths.Log(), *lines, *follow)
	case "plan":
		service, shell, err := in.Plan(ctx)
		if err != nil {
			return err
		}
		data, err := service.Plist()
		if err != nil {
			return err
		}
		if shell.Problem != "" {
			c.note("%s\n", shell.Problem)
		}
		_, err = c.stdout.Write(data)
		return err
	}
	return fail(exitUsage, "unknown service command %q: install, uninstall, start, stop, restart, status, logs or plan", sub)
}

func (c *cli) serviceInstall(ctx context.Context, in installation.Installer, start bool) error {
	previous := endpointToken(in.Paths.Endpoint())
	if !c.json {
		c.note("Installing the Lazybox Runner service from %s…\n", in.Layout.Root)
	}
	result, err := in.Install(ctx, start)
	if err != nil {
		return err
	}
	if result.Shell.Problem != "" && !c.json {
		c.note("%s %s; agents get a standard PATH (see `lazybox doctor`).\n", c.paint(yellow, "!"), result.Shell.Problem)
	}
	report := map[string]any{"version": result.Service.Version, "plist": in.Paths.Plist, "endpoint": in.Paths.Endpoint(), "log": in.Paths.Log(), "started": result.Started, "restarted": result.Restarted}
	if result.Shell.Problem != "" {
		// The app's setup window shows it: agents then get a standard PATH,
		// which may lack the person's tools.
		report["shellProblem"] = result.Shell.Problem
		report["shell"] = result.Shell.Shell
	}
	if result.Previous != nil {
		report["previousVersion"] = result.Previous.Version
	}
	if !result.Started {
		if c.json {
			return c.printJSON(report)
		}
		c.out("%s Installed the Runner service (%s). Start it with `lazybox service start`.\n", c.paint(green, "✓"), result.Service.Version)
		return nil
	}
	runnerVersion, err := c.setup().ready(ctx, in.Paths.Endpoint(), previous)
	if err != nil {
		return fail(exitUnavailable, "the Runner service was installed but did not answer: %v\nSee `lazybox service logs` for why, and `lazybox doctor`.", err)
	}
	report["runnerVersion"] = runnerVersion
	kept := c.liveProcesses(in.Paths.Endpoint())
	report["kept"] = kept
	if c.json {
		return c.printJSON(report)
	}
	switch {
	case result.Previous != nil && result.Previous.Version != "" && result.Previous.Version != result.Service.Version:
		c.out("%s Updated the Runner from %s to %s.\n", c.paint(green, "✓"), result.Previous.Version, runnerVersion)
	case result.Restarted:
		c.out("%s Reinstalled and restarted the Runner (%s).\n", c.paint(green, "✓"), runnerVersion)
	default:
		c.out("%s The Runner (%s) is running. It starts when you log in and keeps working when the Lazybox window is closed.\n", c.paint(green, "✓"), runnerVersion)
	}
	if result.Restarted && kept.total() > 0 {
		c.out("  Kept running and re-attached: %s.\n", kept)
	}
	return nil
}

func (c *cli) reportReady(ctx context.Context, in installation.Installer, previous, what string) error {
	runnerVersion, err := c.setup().ready(ctx, in.Paths.Endpoint(), previous)
	if err != nil {
		return fail(exitUnavailable, "the Runner did not answer: %v\nSee `lazybox service logs`.", err)
	}
	if c.json {
		return c.printJSON(map[string]string{"runnerVersion": runnerVersion})
	}
	c.out("%s %s (%s). Agents, commands and terminals were kept.\n", c.paint(green, "✓"), what, runnerVersion)
	return nil
}

func (c *cli) serviceStatus(ctx context.Context, in installation.Installer) serviceReport {
	r := serviceReport{}
	status, err := in.Status(ctx)
	r.Status = status
	r.Unstable = in.Layout.Unstable()
	if v, verr := in.Layout.ReadVersion(); verr == nil {
		r.ReleaseVersion = v
	}
	switch {
	case err != nil:
		r.Problem = err.Error()
	case !status.Installed:
		r.Problem = "the Runner service is not installed; run `lazybox service install` (the app offers it on first launch)"
	case !status.Loaded:
		r.Problem = "the Runner service is installed but not loaded; run `lazybox service start`"
	}
	if t, err := loadTarget(in.Paths.RunnerName(), in.Paths.Endpoint(), false); err == nil {
		r.URL = t.URL
		probe, cancel := context.WithTimeout(ctx, probeTimeout)
		v, verr := dial(t).Version(probe, connect.NewRequest(&pb.VersionRequest{}))
		cancel()
		if verr == nil {
			r.Reachable, r.RunnerVersion = true, v.Msg.Version
		} else if r.Problem == "" {
			r.Problem = "the Runner does not answer at " + t.URL + "; see `lazybox service logs`"
		}
	} else if r.Problem == "" {
		r.Problem = "the Runner has not written its endpoint yet; see `lazybox service logs`"
	}
	return r
}

func (c *cli) printServiceStatus(r serviceReport) {
	c.out("%s %s\n", c.paint(bold, "Lazybox Runner service"), c.paint(dim, "("+r.Label+")"))
	state := c.paint(dim, "○ not installed")
	switch {
	case r.Running:
		state = c.paint(green, "● running") + fmt.Sprintf(" · pid %d", r.PID)
	case r.Loaded:
		state = c.paint(yellow, "◐ loaded, not running")
		if r.LastExit != "" {
			state += " · last exit " + r.LastExit
		}
	case r.Installed:
		state = c.paint(yellow, "○ installed, not loaded")
	}
	c.out("  %s\n", state)
	if r.Version != "" {
		c.out("  Release   %s\n", r.Version)
	}
	if r.Reachable {
		c.out("  Answers   %s (Runner %s)\n", r.URL, r.RunnerVersion)
	}
	c.out("  State     %s\n  Logs      %s\n", r.State, r.Log)
	if r.Problem != "" {
		c.out("  %s %s\n", c.paint(yellow, "!"), r.Problem)
	}
}

func (c *cli) serviceLogs(path string, lines int, follow bool) error {
	f, err := os.Open(path)
	if errors.Is(err, os.ErrNotExist) {
		return fail(exitNotFound, "no Runner log yet at %s; the service writes it once it starts", path)
	}
	if err != nil {
		return err
	}
	defer f.Close()
	tail, err := lastLines(f, lines)
	if err != nil {
		return err
	}
	for _, line := range tail {
		c.out("%s\n", line)
	}
	if !follow {
		return nil
	}
	if _, err = f.Seek(0, io.SeekEnd); err != nil {
		return err
	}
	reader := bufio.NewReader(f)
	for {
		line, err := reader.ReadString('\n')
		if line != "" {
			c.out("%s", line)
		}
		if err == io.EOF {
			select {
			case <-c.ctx.Done():
				return nil
			case <-time.After(250 * time.Millisecond):
			}
			continue
		}
		if err != nil {
			return err
		}
	}
}

// lastLines reads the last n lines of a log, reading at most its last 4 MiB.
func lastLines(f *os.File, n int) ([]string, error) {
	info, err := f.Stat()
	if err != nil {
		return nil, err
	}
	start := info.Size() - 4<<20
	if start < 0 {
		start = 0
	}
	if _, err = f.Seek(start, io.SeekStart); err != nil {
		return nil, err
	}
	data, err := io.ReadAll(f)
	if err != nil {
		return nil, err
	}
	all := strings.Split(strings.TrimRight(string(data), "\n"), "\n")
	if len(all) == 1 && all[0] == "" {
		return nil, nil
	}
	if n > 0 && len(all) > n {
		all = all[len(all)-n:]
	}
	return all, nil
}

// endpointToken is the bearer the endpoint file holds now, so a wait can tell
// the next Runner's file from the previous one's.
func endpointToken(path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	var e endpoint
	if json.Unmarshal(data, &e) != nil {
		return ""
	}
	return e.Token
}

// waitForRunner waits until the endpoint file names a Runner other than the
// one that held previousToken and that Runner answers Version.
func waitForRunner(ctx context.Context, path, previousToken string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 90*time.Second)
	defer cancel()
	last := errors.New("no endpoint file yet")
	for {
		if t, err := loadTarget("", path, false); err != nil {
			last = err
		} else if t.Token == previousToken {
			last = errors.New("the previous Runner's endpoint is still in place")
		} else {
			probe, done := context.WithTimeout(ctx, 2*time.Second)
			v, err := dial(t).Version(probe, connect.NewRequest(&pb.VersionRequest{}))
			done()
			if err == nil {
				return v.Msg.Version, nil
			}
			last = err
		}
		select {
		case <-ctx.Done():
			return "", fmt.Errorf("%w (last: %v)", ctx.Err(), last)
		case <-time.After(200 * time.Millisecond):
		}
	}
}

// hosted counts what runs in session hosts: what a Runner restart keeps.
type hosted struct {
	Agents    int `json:"agents"`
	Commands  int `json:"commands"`
	Terminals int `json:"terminals"`
}

func (h hosted) total() int { return h.Agents + h.Commands + h.Terminals }

func (h hosted) String() string {
	var parts []string
	add := func(n int, one, many string) {
		if n == 1 {
			parts = append(parts, "1 "+one)
		} else if n > 1 {
			parts = append(parts, fmt.Sprintf("%d %s", n, many))
		}
	}
	add(h.Agents, "agent", "agents")
	add(h.Commands, "command", "commands")
	add(h.Terminals, "terminal", "terminals")
	switch len(parts) {
	case 0:
		return "nothing"
	case 1:
		return parts[0]
	}
	return strings.Join(parts[:len(parts)-1], ", ") + " and " + parts[len(parts)-1]
}

// liveProcesses asks the Runner what it hosts, best effort.
func (c *cli) liveProcesses(endpointPath string) hosted {
	var h hosted
	t, err := loadTarget("", endpointPath, false)
	if err != nil {
		return h
	}
	ctx, cancel := context.WithTimeout(c.ctx, 3*time.Second)
	defer cancel()
	inventory, err := dial(t).ListProcesses(ctx, connect.NewRequest(&pb.ListProcessesRequest{}))
	if err != nil {
		return h
	}
	for _, p := range inventory.Msg.Processes {
		if p.State == "exited" {
			continue
		}
		switch p.Kind {
		case "agent":
			h.Agents++
		case "command":
			h.Commands++
		case "terminal":
			h.Terminals++
		}
	}
	return h
}
