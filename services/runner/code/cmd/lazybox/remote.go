package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/link"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// Runners on other computers (REMOTE_RUNNERS.md, P0): the owner link over SSH.
//
// On the host (run by sshd as the person's own login):
//
//	lazybox link serve --stdio      the link endpoint on stdin/stdout; never a listener
//	lazybox link owners ls|add|revoke  the client keys with owner access
//	lazybox link up                 make sure the Runner runs (service, or detached)
//
// On the client (this computer):
//
//	lazybox remote add <ssh-host>   probe, install, start, enrol, pin
//	lazybox remote ls | rm <name> | hosts
//	lazybox link connect <name>     the link agent: the remote Runner on a loopback origin

// linkSubcommands are `lazybox link`'s remote verbs; anything else is the
// PATH link or a pull request URL (link.go).
var linkSubcommands = map[string]bool{"serve": true, "connect": true, "owners": true, "up": true}

func (c *cli) linkRemote(args []string) error {
	switch args[0] {
	case "serve":
		return c.linkServe(args[1:])
	case "connect":
		return c.linkConnect(args[1:])
	case "owners":
		return c.linkOwners(args[1:])
	case "up":
		return c.linkUp(args[1:])
	}
	return fail(exitUsage, "unknown link verb %q", args[0])
}

// hostRunner is the Runner on this computer a link serves: its state
// directory and endpoint file. Never from inside an agent, a workspace
// terminal or a hook: their scoped credential is not an owner's, and owner
// keys are not theirs to add.
func (c *cli) hostRunner() (state, endpoint, name string, err error) {
	if scopedEnvironment(c.getenv) {
		return "", "", "", fail(exitDenied, "owner links are not available from inside Lazybox (an agent, a workspace terminal or a hook)")
	}
	t, err := resolveTarget(c.getenv, c.endpoint, c.runner)
	if err != nil {
		return "", "", "", err
	}
	state = filepath.Dir(t.EndpointPath)
	if locators, err := readLocators(c.getenv); err == nil {
		for _, l := range locators {
			if l.Name == t.Name && l.State != "" {
				state = l.State
			}
		}
	}
	return state, t.EndpointPath, t.Name, nil
}

func machineName() string {
	name, _ := os.Hostname()
	name = strings.TrimSuffix(name, ".local")
	if name == "" {
		name = "this computer"
	}
	return name
}

// stdio is this process's stdin and stdout as one stream.
type stdio struct {
	io.Reader
	io.Writer
}

func (s stdio) Close() error {
	var errs []error
	for _, v := range []any{s.Reader, s.Writer} {
		if c, ok := v.(io.Closer); ok {
			errs = append(errs, c.Close())
		}
	}
	return errors.Join(errs...)
}

// processStdio is this process's stdin and stdout made pollable, so closing
// the stream ends a read in progress at once. With the blocking descriptors
// sshd hands over, a revoked key's session would sit in a read until the
// client next wrote, and sshd keeps the channel open until this process exits.
func processStdio() stdio {
	if syscall.SetNonblock(0, true) != nil || syscall.SetNonblock(1, true) != nil {
		return stdio{Reader: os.Stdin, Writer: os.Stdout}
	}
	// os.NewFile registers a non-blocking descriptor with the runtime's poller.
	return stdio{Reader: os.NewFile(0, "stdin"), Writer: os.NewFile(1, "stdout")}
}

func (c *cli) linkServe(args []string) error {
	f := c.flags("link serve")
	useStdio := f.Bool("stdio", false, "serve one link on stdin and stdout (what ssh runs)")
	route := f.String("route", "ssh", "what carries the stream, for the audit")
	probe := f.Bool("probe", false, "print the link protocol this lazybox speaks, and exit")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	if *probe {
		c.out("lazybox-link 1\n")
		return nil
	}
	if !*useStdio {
		// The only endpoint is stdio: a Runner never listens beyond loopback.
		return fail(exitUsage, "lazybox link serve serves stdin and stdout only (--stdio), for `ssh <host> lazybox link serve --stdio`; it opens no port")
	}
	state, endpoint, _, err := c.hostRunner()
	if err != nil {
		fmt.Fprintf(c.stderr, "lazybox-link: unavailable: %v\n", err)
		return err
	}
	peer := ""
	if fields := strings.Fields(c.getenv("SSH_CONNECTION")); len(fields) >= 2 {
		peer = fields[0] + ":" + fields[1]
	}
	stream := stdio{Reader: c.stdin, Writer: c.stdout}
	if c.stdin == os.Stdin && c.stdout == os.Stdout {
		stream = processStdio()
	}
	err = link.Serve(c.ctx, stream, link.ServeOptions{State: state, Endpoint: endpoint, Route: *route, Peer: peer, Machine: machineName(), Release: version.String(), Log: c.stderr})
	var refusal *link.Refusal
	if errors.As(err, &refusal) {
		return exitError{exitDenied, errors.New("link refused: " + refusal.Reason)}
	}
	return err
}

func (c *cli) linkOwners(args []string) error {
	f := c.flags("link owners")
	name := f.String("name", "", "add: the client's name (\"MacBook Pro\")")
	channel := f.String("channel-key", "", "add: the client's X25519 channel public key (base64url)")
	identity := f.String("identity-key", "", "add: the client's P-256 identity public key (PKIX, base64url)")
	via := f.String("via", "cli", "add: how the key came here (ssh, kubernetes, cli)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	sub := "ls"
	if len(positional) > 0 {
		sub, positional = positional[0], positional[1:]
	}
	state, endpoint, _, err := c.hostRunner()
	if err != nil {
		return err
	}
	owners := link.Owners{State: state}
	audit := link.Audit{State: state}
	by := firstWord(c.getenv("SSH_CONNECTION"))
	if by == "" {
		by = "this computer"
	}
	switch sub {
	case "ls", "list":
		list, err := owners.List()
		if err != nil {
			return err
		}
		key, err := link.LoadOrCreateRunnerKey(state)
		if err != nil {
			return err
		}
		if c.json {
			if list == nil {
				list = []link.Owner{}
			}
			return c.printJSON(map[string]any{"runnerKey": link.Encode(key.Public), "fingerprint": link.Fingerprint(key.Public), "owners": list})
		}
		c.out("This Runner's link key: %s\n", link.Fingerprint(key.Public))
		if len(list) == 0 {
			c.out("No computer has owner access over a link yet. `lazybox remote add %s` on another computer adds one.\n", machineName())
			return nil
		}
		for _, o := range list {
			c.out("  %s  %-24s added via %s, %s\n", o.KeyID, o.Name, o.AddedVia, o.AddedAt.Local().Format("2006-01-02 15:04"))
		}
		return nil
	case "add":
		ch, err1 := link.Decode(*channel)
		id, err2 := link.Decode(*identity)
		if err := errors.Join(err1, err2); err != nil || *channel == "" || *identity == "" {
			return fail(exitUsage, "lazybox link owners add needs --name, --channel-key and --identity-key (base64url)")
		}
		key, err := link.LoadOrCreateRunnerKey(state)
		if err != nil {
			return err
		}
		owner, err := owners.Add(*name, ch, id, *via, time.Now())
		if err != nil {
			return fail(exitUsage, "%v", err)
		}
		audit.Record(link.AuditEntry{Event: "owner.added", KeyID: owner.KeyID, Name: owner.Name, Route: *via, By: by})
		runnerID := ""
		if up, err := link.ReadEndpoint(endpoint); err == nil {
			runnerID = up.RunnerID
		}
		e := link.Enrolment{KeyID: owner.KeyID, RunnerKey: link.Encode(key.Public), Fingerprint: link.Fingerprint(key.Public), RunnerID: runnerID, Machine: machineName(), Release: version.String()}
		if c.json {
			return c.printJSON(e)
		}
		c.out("%s %s (%s) is an owner of this Runner. It pins this Runner's key %s.\n", c.paint(green, "✓"), owner.Name, owner.KeyID, e.Fingerprint)
		return nil
	case "revoke", "rm":
		if len(positional) != 1 {
			return fail(exitUsage, "lazybox link owners revoke [--] KEY-ID (lazybox link owners ls lists them; put -- before an ID that begins with -)")
		}
		owner, err := owners.Revoke(positional[0])
		if err != nil {
			return fail(exitNotFound, "%v", err)
		}
		audit.Record(link.AuditEntry{Event: "owner.revoked", KeyID: owner.KeyID, Name: owner.Name, By: by})
		if c.json {
			return c.printJSON(map[string]any{"revoked": owner})
		}
		c.out("%s Revoked %s (%s). Its links end within a second.\n", c.paint(green, "✓"), owner.Name, owner.KeyID)
		return nil
	}
	return fail(exitUsage, "lazybox link owners ls|add|revoke")
}

func firstWord(s string) string {
	if fields := strings.Fields(s); len(fields) > 0 {
		return fields[0]
	}
	return ""
}

// linkUp makes sure this computer's Runner runs, for `remote add` over SSH:
// the registered one if it answers; otherwise the installed release's
// service (launchd, a systemd user unit, or Lazybox's supervisor), or, when
// that cannot be installed, the Runner started detached from this session
// (it survives the SSH login but not a reboot, and says so).
func (c *cli) linkUp(args []string) error {
	f := c.flags("link up")
	service := f.String("service", "auto", "auto: the platform's service when Lazybox can install one, else none; none: start the Runner detached")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	report := link.Up{State: "starting"}
	answer := func(err error) error {
		if err != nil {
			report.State, report.Problem = "failed", err.Error()
		}
		if c.json {
			if printErr := c.printJSON(report); printErr != nil {
				return printErr
			}
			if err != nil {
				return exitError{exitUnavailable, err}
			}
			return nil
		}
		if err != nil {
			return err
		}
		c.out("%s The Runner runs (%s, %s).\n", c.paint(green, "✓"), report.Service, report.Version)
		return nil
	}
	if scopedEnvironment(c.getenv) {
		return answer(errors.New("not from inside Lazybox"))
	}
	// Already running: nothing to do.
	if t, err := resolveTarget(c.getenv, c.endpoint, c.runner); err == nil {
		if v, err := c.probeRunner(t.EndpointPath, 3*time.Second); err == nil {
			report.State, report.Service, report.Runner, report.Endpoint, report.Version = "running", "existing", t.Name, t.EndpointPath, v
			return answer(nil)
		}
	}
	paths, err := installation.UserPaths(c.getenv)
	if err != nil {
		return answer(err)
	}
	report.Runner, report.Endpoint = paths.RunnerName(), paths.Endpoint()
	// A full release gets its platform's service (`lazybox service install`):
	// launchd on a Mac, a systemd user unit on Linux, Lazybox's own
	// supervisor on a Linux host without systemd (a container). Agents
	// outlive a restart under each (SESSION_HOSTS.md). When the service
	// cannot be installed (launchd over SSH needs a GUI login on that Mac; a
	// Linux login without a user manager) the Runner starts detached instead.
	if *service == "auto" {
		if layout, err := installation.Current(); err == nil && layout.Check() == nil {
			if installed, err := c.installServiceForLink(); err == nil {
				if v, err := c.probeRunner(paths.Endpoint(), 60*time.Second); err == nil {
					// Under systemd without lingering this says the Runner stops
					// when the person logs out of that computer, and the command
					// that keeps it running; remote add shows it.
					report.State, report.Service, report.Version, report.StartsWhen = "running", installed.Manager, v, installed.StartsWhen
					return answer(nil)
				}
			} else {
				report.Note = "the service could not be installed (" + err.Error() + "); the Runner was started detached"
			}
		}
	}
	pid, err := startDetached(paths)
	if err != nil {
		return answer(err)
	}
	report.PID = pid
	v, err := c.probeRunner(paths.Endpoint(), 60*time.Second)
	if err != nil {
		return answer(fmt.Errorf("the Runner started but does not answer: %v (see %s)", err, filepath.Join(paths.Logs, "runner.log")))
	}
	report.State, report.Service, report.Version = "running", "none", v
	return answer(nil)
}

// serviceInstalled is what `lazybox service install --json` reports that a
// link needs: which manager runs the Runner and when it starts by itself.
type serviceInstalled struct {
	Manager    string `json:"manager"`
	StartsWhen string `json:"startsWhen"`
}

// installServiceForLink installs (or updates) this release's Runner service
// in a process of its own, as a person would type it, and says how it runs.
func (c *cli) installServiceForLink() (serviceInstalled, error) {
	exe, err := os.Executable()
	if err != nil {
		return serviceInstalled{}, err
	}
	var stdout, stderr strings.Builder
	cmd := exec.CommandContext(c.ctx, exe, "service", "install", "--json")
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err = cmd.Run(); err != nil {
		why := strings.TrimSpace(stderr.String())
		if why == "" {
			why = err.Error()
		}
		return serviceInstalled{}, errors.New(lastLineOf(why))
	}
	var installed serviceInstalled
	if err = json.Unmarshal([]byte(stdout.String()), &installed); err != nil {
		return serviceInstalled{}, fmt.Errorf("lazybox service install said %q", strings.TrimSpace(stdout.String()))
	}
	return installed, nil
}

func lastLineOf(text string) string {
	lines := strings.Split(strings.TrimSpace(text), "\n")
	return lines[len(lines)-1]
}

// probeRunner waits until the endpoint file names a Runner that answers.
func (c *cli) probeRunner(endpoint string, limit time.Duration) (string, error) {
	ctx, cancel := context.WithTimeout(c.ctx, limit)
	defer cancel()
	return waitForRunner(ctx, endpoint, "")
}

// startDetached starts lazybox-runner beside this lazybox in a session of
// its own, registers it for the CLI (the locator `link serve` reads), and
// returns at once. It keeps agents across its own restarts as the service does.
func startDetached(paths installation.Paths) (int, error) {
	exe, err := os.Executable()
	if err != nil {
		return 0, err
	}
	if resolved, err := filepath.EvalSymlinks(exe); err == nil {
		exe = resolved
	}
	runner := filepath.Join(filepath.Dir(exe), "lazybox-runner")
	if _, err := os.Stat(runner); err != nil {
		return 0, fmt.Errorf("no lazybox-runner beside %s: install the Runner bundle first", exe)
	}
	for _, dir := range []string{paths.State, paths.Logs, filepath.Dir(paths.Config)} {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return 0, err
		}
	}
	log, err := os.OpenFile(filepath.Join(paths.Logs, "runner.log"), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return 0, err
	}
	defer log.Close()
	// The Runner finds its worker, Node and web app in the runtime it ships in.
	args := []string{"serve", "--state", paths.State, "--config", paths.Config, "--worktrees", paths.Worktrees, "--actions", paths.ActionsFile()}
	cmd := exec.Command(runner, args...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = nil, log, log
	cmd.Env = os.Environ()
	// Its own session: the SSH login ending does not end it.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err = cmd.Start(); err != nil {
		return 0, err
	}
	pid := cmd.Process.Pid
	_ = cmd.Process.Release()
	_ = os.WriteFile(filepath.Join(paths.State, "runner.pid"), []byte(fmt.Sprintln(pid)), 0o600)
	return pid, installation.Installer{Paths: paths}.WriteLocator()
}

// ---- the client side ----

func (c *cli) remotes() link.Remotes { return link.Remotes{Config: configDir(c.getenv)} }

func (c *cli) remote(args []string) error {
	if len(args) == 0 {
		args = []string{"ls"}
	}
	switch args[0] {
	case "add":
		return c.remoteAdd(args[1:])
	case "add-kubernetes":
		return c.remoteAddKubernetes(args[1:])
	case "ls", "list":
		return c.remoteList(args[1:])
	case "rm", "remove":
		return c.remoteRemove(args[1:])
	case "hosts":
		return c.remoteHosts(args[1:])
	}
	return fail(exitUsage, "lazybox remote add <ssh-host> | add-kubernetes | ls | rm <name> | hosts")
}

// progress reports `remote add`'s steps: one JSON line each (--progress json,
// what the desktop's Add a computer reads) or a line of text.
type progress struct {
	c    *cli
	json bool
}

type progressEvent struct {
	Step   string `json:"step"`
	Title  string `json:"title"`
	State  string `json:"state"`
	Detail string `json:"detail,omitempty"`
}

func (p progress) emit(step, title, state, detail string) {
	if p.json {
		data, _ := json.Marshal(progressEvent{Step: step, Title: title, State: state, Detail: detail})
		fmt.Fprintln(p.c.stdout, string(data))
		return
	}
	mark := map[string]string{"done": p.c.paint(green, "✓"), "failed": p.c.paint(red, "✗"), "skipped": p.c.paint(dim, "–")}[state]
	if mark == "" {
		return
	}
	line := "  " + mark + " " + title
	if detail != "" {
		line += p.c.paint(dim, "  "+detail)
	}
	fmt.Fprintln(p.c.stderr, line)
}

func (c *cli) remoteAdd(args []string) error {
	f := c.flags("remote add")
	name := f.String("name", "", "the name this computer's Runner goes by here (default: the host's)")
	bundle := f.String("bundle", "", "a Runner bundle (.tar.gz) to install on the host, for its OS and CPU")
	sha := f.String("sha256", "", "the bundle's expected SHA-256 (checked here and on the host)")
	service := f.String("service", "auto", "how the host runs its Runner: auto or none (started detached, not at boot)")
	mode := f.String("progress", "text", "text, or json: one line per step")
	upgrade := f.Bool("upgrade", false, "install the bundle even when the host has a Lazybox that serves links")
	noConnect := f.Bool("no-connect", false, "do not open a link to check it at the end")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "lazybox remote add <ssh-host>: a Host from ~/.ssh/config, or user@host")
	}
	host := positional[0]
	if !link.ValidHost(host) {
		return fail(exitUsage, "%q is not an ssh host", host)
	}
	if *name == "" {
		*name = link.NameFor(host)
	}
	if err := link.CheckName(*name); err != nil {
		return fail(exitUsage, "%v", err)
	}
	remotes := c.remotes()
	if existing, err := remotes.Load(*name); err == nil && (existing.Route == "kubernetes" || existing.Host != host) {
		return fail(exitUsage, "%s already names %s; choose another --name, or `lazybox remote rm %s` first", *name, existing.Host, *name)
	}
	p := progress{c: c, json: *mode == "json"}
	route := link.SSHRoute{Host: host, ControlDir: remotes.ControlDir(), Interactive: !p.json && c.stdinTTY}
	failed := func(step, title string, err error) error {
		p.emit(step, title, "failed", err.Error())
		if p.json {
			data, _ := json.Marshal(map[string]string{"error": err.Error()})
			fmt.Fprintln(c.stdout, string(data))
			return exitError{exitUnavailable, err}
		}
		return fail(exitUnavailable, "%v", err)
	}

	p.emit("connect", "Connect to "+host, "running", "")
	ctx, cancel := context.WithTimeout(c.ctx, link.StepTimeout())
	probe, err := link.RunProbe(ctx, route)
	cancel()
	if err != nil {
		return failed("connect", "Connect to "+host, err)
	}
	p.emit("connect", "Connect to "+host, "done", "")
	p.emit("probe", "Check the host", "done", probe.Summary())

	needs := probe.Lazybox == "" || probe.Link != 1 || *upgrade
	var b link.Bundle
	if *bundle != "" {
		if b, err = link.OpenBundle(*bundle, "", *sha); err != nil {
			return failed("install", "Install the Runner", err)
		}
		needs = needs || probe.Version != b.Version
	}
	command := probe.Lazybox
	if needs {
		title := "Upload and verify the Runner"
		if b.Path == "" {
			local := runtime.GOOS + "-" + runtime.GOARCH
			layout, lerr := installation.Current()
			if probe.GoPlatform() != local || lerr != nil {
				err = fmt.Errorf("%s runs %s %s and has no Lazybox that serves links. This computer has no Runner build for it: build one (a lazybox-runner .tar.gz for %s) and pass --bundle FILE", host, probe.OS, probe.Arch, probe.GoPlatform())
				if lerr != nil && probe.GoPlatform() == local {
					err = fmt.Errorf("%s needs a Lazybox Runner, and this lazybox is a development build, not an installed release: pass --bundle FILE (a runtime .tar.gz)", host)
				}
				return failed("install", title, err)
			}
			tmp, err := os.MkdirTemp("", "lazybox-bundle-")
			if err != nil {
				return failed("install", title, err)
			}
			defer os.RemoveAll(tmp)
			p.emit("install", title, "running", "packing this computer's "+version.String())
			if b, err = link.PackRuntime(layout.Root, tmp); err != nil {
				return failed("install", title, err)
			}
		}
		title = "Upload " + b.Version + " and verify"
		p.emit("install", title, "running", "SHA-256 "+b.SHA256[:12]+"…")
		ictx, cancel := context.WithTimeout(c.ctx, 20*time.Minute)
		command, err = link.Install(ictx, route, b)
		cancel()
		if err != nil {
			return failed("install", title, err)
		}
		p.emit("install", title, "done", "SHA-256 "+b.SHA256[:12]+"… checked on "+host)
	} else {
		p.emit("install", "Install the Runner", "skipped", "Lazybox "+probe.Version+" is installed")
	}
	route.Remote = command

	p.emit("start", "Start the Runner", "running", "")
	ctx, cancel = context.WithTimeout(c.ctx, link.StepTimeout())
	up, err := link.StartRunner(ctx, route, *service)
	cancel()
	if err != nil {
		return failed("start", "Start the Runner", err)
	}
	how := map[string]string{"existing": "already running", installation.ManagerLaunchd: "as a launchd service", installation.ManagerSystemd: "as a systemd user service",
		installation.ManagerSupervisor: "under Lazybox's supervisor", "none": "started for this login (not at boot)"}[up.Service]
	if up.StartsWhen != "" {
		how += "; " + up.StartsWhen
	}
	if up.Note != "" {
		how += "; " + up.Note
	}
	p.emit("start", "Start the Runner", "done", strings.TrimSpace(up.Version+" "+how))
	route.Runner = up.Runner

	me := machineName()
	p.emit("enrol", "Add "+me+" as owner", "running", "")
	keys, err := remotes.Keys().Keys()
	if err != nil {
		return failed("enrol", "Add "+me+" as owner", err)
	}
	ctx, cancel = context.WithTimeout(c.ctx, link.StepTimeout())
	enrolment, err := link.Enrol(ctx, route, keys, me)
	cancel()
	if err != nil {
		return failed("enrol", "Add "+me+" as owner", err)
	}
	r := link.Remote{Name: *name, Route: "ssh", Host: host, Command: command, Runner: up.Runner, RunnerKey: enrolment.RunnerKey, Fingerprint: enrolment.Fingerprint,
		RunnerID: enrolment.RunnerID, Machine: firstNonEmptyString(enrolment.Machine, probe.Hostname), OS: probe.OS, Arch: probe.Arch, Release: enrolment.Release, KeyID: enrolment.KeyID, AddedAt: time.Now().UTC()}
	if err = remotes.Save(r); err != nil {
		return failed("enrol", "Add "+me+" as owner", err)
	}
	p.emit("enrol", "Add "+me+" as owner", "done", "pinned "+enrolment.Fingerprint)
	// A link agent already running for it (refused while the key was revoked) tries again now.
	if st, alive := remotes.Status(r.Name); alive && st.PID > 0 {
		_ = syscall.Kill(st.PID, syscall.SIGUSR1)
	}

	if !*noConnect {
		p.emit("connected", "Connect over the link", "running", "")
		pin, _ := r.PinnedKey()
		ctx, cancel = context.WithTimeout(c.ctx, link.StepTimeout())
		started := time.Now()
		session, err := link.Dial(ctx, route, keys, pin, me)
		if err == nil {
			var v *connect.Response[pb.VersionResponse]
			v, err = genconnect.NewRunnerServiceClient(session.HTTPClient(), "http://runner").Version(ctx, connect.NewRequest(&pb.VersionRequest{}))
			_ = session.Close()
			if err == nil {
				p.emit("connected", "Connect over the link", "done", fmt.Sprintf("Runner %s answered in %s", v.Msg.Version, time.Since(started).Round(time.Millisecond)))
			}
		}
		cancel()
		if err != nil {
			return failed("connected", "Connect over the link", err)
		}
	}
	if p.json {
		data, _ := json.Marshal(map[string]any{"done": true, "remote": r})
		fmt.Fprintln(c.stdout, string(data))
		return nil
	}
	if c.json {
		return c.printJSON(r)
	}
	c.out("%s %s is added as %q. Lazybox shows it in the sidebar; from a terminal: lazybox link connect %s, then lazybox --runner %s ls.\n", c.paint(green, "✓"), host, r.Name, r.Name, r.Name)
	return nil
}

func (c *cli) remoteAddKubernetes(args []string) error {
	f := c.flags("remote add-kubernetes")
	name := f.String("name", "", "local Runner name")
	var target link.KubernetesTarget
	f.StringVar(&target.Context, "context", "", "Kubernetes context")
	f.StringVar(&target.Namespace, "namespace", "", "Runner namespace")
	f.StringVar(&target.Workload, "workload", "", "deployment/name or statefulset/name")
	f.StringVar(&target.Container, "container", "", "Runner container name")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 0 {
		return fail(exitUsage, "lazybox remote add-kubernetes --name NAME --context CONTEXT --namespace NS --workload KIND/NAME --container CONTAINER")
	}
	if err = link.CheckName(*name); err != nil {
		return fail(exitUsage, "%v", err)
	}
	if err = target.Validate(); err != nil {
		return fail(exitUsage, "%v", err)
	}
	target.Kubectl, err = exec.LookPath("kubectl")
	if err != nil {
		return fmt.Errorf("find kubectl: %w", err)
	}
	target.Kubectl, err = filepath.Abs(target.Kubectl)
	if err != nil {
		return err
	}
	remotes := c.remotes()
	if _, err = os.Stat(filepath.Join(remotes.Dir(*name), "remote.json")); !errors.Is(err, os.ErrNotExist) {
		if err != nil {
			return err
		}
		return fail(exitUsage, "%s already exists; choose a different --name", *name)
	}
	keys, err := remotes.Keys().Keys()
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(c.ctx, link.StepTimeout())
	defer cancel()
	route := link.KubernetesRoute{Target: target}
	// The container's image installs Lazybox: it is checked, never installed into a pod that would lose it.
	probe, err := link.RunProbe(ctx, route)
	if err != nil {
		return err
	}
	if probe.Lazybox == "" || probe.Link != 1 {
		return fmt.Errorf("%s has no Lazybox that serves links (%s): run a Runner image that includes it", route.Describe().Host, probe.Summary())
	}
	enrolment, err := link.Enrol(ctx, route, keys, machineName())
	if err != nil {
		return err
	}
	r := link.Remote{Name: *name, Route: route.Kind(), Host: route.Describe().Host, Kubernetes: &target,
		RunnerKey: enrolment.RunnerKey, Fingerprint: enrolment.Fingerprint, RunnerID: enrolment.RunnerID,
		Machine: firstNonEmptyString(enrolment.Machine, probe.Hostname), OS: probe.OS, Arch: probe.Arch, Release: enrolment.Release, KeyID: enrolment.KeyID, AddedAt: time.Now().UTC()}
	if err = remotes.Save(r); err != nil {
		return err
	}
	pin, err := r.PinnedKey()
	if err != nil {
		return err
	}
	session, err := link.Dial(ctx, route, keys, pin, machineName())
	if err != nil {
		return fmt.Errorf("saved %s but the first connection failed: %w", r.Name, err)
	}
	defer session.Close()
	if _, err = genconnect.NewRunnerServiceClient(session.HTTPClient(), "http://runner").Version(ctx, connect.NewRequest(&pb.VersionRequest{})); err != nil {
		return fmt.Errorf("saved %s but its Runner did not answer: %w", r.Name, err)
	}
	if c.json {
		return c.printJSON(r)
	}
	c.out("Added %s. Connect with lazybox link connect %s.\n", r.Name, r.Name)
	return nil
}

func firstNonEmptyString(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

type remoteRow struct {
	link.Remote
	State     string `json:"state"`
	RTTMillis int64  `json:"rttMs,omitempty"`
	Problem   string `json:"problem,omitempty"`
	URL       string `json:"url,omitempty"`
}

func (c *cli) remoteList(args []string) error {
	f := c.flags("remote ls")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	remotes := c.remotes()
	list, err := remotes.List()
	if err != nil {
		return err
	}
	rows := []remoteRow{}
	for _, r := range list {
		row := remoteRow{Remote: r, State: "not connected"}
		if st, alive := remotes.Status(r.Name); alive {
			row.State, row.RTTMillis, row.Problem = st.State, st.RTTMillis, st.Problem
			if up, err := link.ReadEndpoint(remotes.Endpoint(r.Name)); err == nil {
				row.URL = up.URL
			}
		}
		rows = append(rows, row)
	}
	if c.json {
		return c.printJSON(map[string]any{"remotes": rows})
	}
	if len(rows) == 0 {
		c.out("No other computers yet. `lazybox remote add <ssh-host>` adds one (`lazybox remote hosts` lists your ~/.ssh/config).\n")
		return nil
	}
	for _, r := range rows {
		state := r.State
		if r.RTTMillis > 0 {
			state += fmt.Sprintf(" · %d ms", r.RTTMillis)
		}
		c.out("%-16s %-28s %s %s\n", r.Name, r.Host, state, c.paint(dim, strings.TrimSpace(r.OS+" "+r.Arch+" · "+r.Fingerprint)))
		if r.Problem != "" {
			c.out("%16s %s\n", "", c.paint(dim, r.Problem))
		}
	}
	return nil
}

func (c *cli) remoteRemove(args []string) error {
	f := c.flags("remote rm")
	keep := f.Bool("keep-on-host", false, "forget it here only; leave this computer's key enrolled on the host")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "lazybox remote rm <name> (lazybox remote ls)")
	}
	remotes := c.remotes()
	r, err := remotes.Load(positional[0])
	if err != nil {
		return fail(exitNotFound, "%v", err)
	}
	if st, alive := remotes.Status(r.Name); alive && st.PID > 0 {
		_ = syscall.Kill(st.PID, syscall.SIGTERM)
	}
	result := map[string]any{"removed": r.Name, "revoked": false}
	if !*keep {
		ctx, cancel := context.WithTimeout(c.ctx, 30*time.Second)
		route, routeErr := r.OwnerRoute(remotes.ControlDir())
		err = routeErr
		if err == nil {
			err = link.Revoke(ctx, route, r.KeyID)
		}
		cancel()
		if err != nil {
			result["problem"] = stillEnrolled(r.Host, r.KeyID, err)
		} else {
			result["revoked"] = true
		}
	}
	if err = remotes.Remove(r.Name); err != nil {
		return err
	}
	if c.json {
		return c.printJSON(result)
	}
	c.out("%s Forgot %s.\n", c.paint(green, "✓"), r.Name)
	if problem, ok := result["problem"].(string); ok {
		c.note("%s %s\n", c.paint(yellow, "!"), problem)
	} else if !*keep {
		c.out("  Revoked this computer's key on %s.\n", r.Host)
	}
	return nil
}

// stillEnrolled says the key could not be revoked on the host, and the command
// that revokes it there. A key ID is base64url and may begin with "-", so the
// command puts it after `--`, where it is never read as a flag.
func stillEnrolled(host, keyID string, err error) string {
	return "this computer's key is still enrolled on " + host + ": " + err.Error() + ". Revoke it there with `lazybox link owners revoke -- " + keyID + "`."
}

func (c *cli) remoteHosts(args []string) error {
	f := c.flags("remote hosts")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	home := c.getenv("HOME")
	if home == "" {
		home, _ = os.UserHomeDir()
	}
	hosts := link.SSHHosts(home)
	added := map[string]string{}
	if list, err := c.remotes().List(); err == nil {
		for _, r := range list {
			added[r.Host] = r.Name
		}
	}
	type row struct {
		link.SSHHost
		Added string `json:"added,omitempty"`
	}
	rows := []row{}
	for _, h := range hosts {
		rows = append(rows, row{SSHHost: h, Added: added[h.Name]})
	}
	if c.json {
		return c.printJSON(map[string]any{"hosts": rows})
	}
	if len(rows) == 0 {
		c.out("No Host entries in ~/.ssh/config. `lazybox remote add user@host` works too.\n")
		return nil
	}
	for _, r := range rows {
		detail := strings.TrimSpace(strings.Join([]string{r.User, r.HostName}, "@"))
		detail = strings.Trim(detail, "@")
		mark := ""
		if r.Added != "" {
			mark = c.paint(green, " added")
		}
		c.out("%-24s %s%s\n", r.Name, c.paint(dim, detail), mark)
	}
	return nil
}

// linkConnect is the link agent for one remote: it serves that Runner on a
// loopback origin (endpoint.json beside its record, the locator already
// points there) until it is stopped. One per remote: a second exits at once.
func (c *cli) linkConnect(args []string) error {
	f := c.flags("link connect")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "lazybox link connect <name> (lazybox remote ls)")
	}
	remotes := c.remotes()
	r, err := remotes.Load(positional[0])
	if err != nil {
		return fail(exitNotFound, "%v", err)
	}
	pin, err := r.PinnedKey()
	if err != nil {
		return err
	}
	keys, err := remotes.Keys().Keys()
	if err != nil {
		return err
	}
	lock, err := os.OpenFile(filepath.Join(remotes.Dir(r.Name), "link.lock"), os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		st, _ := remotes.Status(r.Name)
		if c.json {
			return c.printJSON(map[string]any{"state": "already-connected", "pid": st.PID})
		}
		c.out("%s is already connected by another lazybox (pid %d).\n", r.Name, st.PID)
		return nil
	}
	route, err := r.OwnerRoute(remotes.ControlDir())
	if err != nil {
		return err
	}
	agent := &link.Agent{Route: route, Keys: keys, RunnerKey: pin, ClientName: machineName(), Label: r.Name}
	origin, token, err := agent.Listen()
	if err != nil {
		return err
	}
	agent.OnStatus = func(s link.Status) {
		_ = remotes.WriteStatus(r.Name, link.LinkStatus{Status: s, PID: os.Getpid(), Host: r.Host, Route: r.Route, At: time.Now().UTC()})
		if c.json {
			data, _ := json.Marshal(map[string]any{"name": r.Name, "url": origin, "state": s.State, "rttMs": s.RTTMillis, "problem": s.Problem})
			fmt.Fprintln(c.stdout, string(data))
		} else if s.State != "connected" || s.RTTMillis == 0 {
			c.note("%s: %s %s\n", r.Name, s.State, s.Problem)
		}
	}
	if err = remotes.WriteEndpoint(r.Name, origin, token, r.RunnerID); err != nil {
		return err
	}
	defer os.Remove(remotes.Endpoint(r.Name))
	// SIGUSR1: the desktop saw a wake or a network change; check the link now.
	wake := make(chan os.Signal, 1)
	signal.Notify(wake, syscall.SIGUSR1)
	defer signal.Stop(wake)
	ctx, cancel := context.WithCancel(c.ctx)
	defer cancel()
	go func() {
		for {
			select {
			case <-ctx.Done():
				return
			case <-wake:
				agent.Wake()
			}
		}
	}()
	if !c.json {
		c.note("%s is served at %s (lazybox --runner %s …). Ctrl+C ends the link; agents on %s keep running.\n", r.Name, origin, r.Name, r.Host)
	}
	return agent.Run(ctx)
}
