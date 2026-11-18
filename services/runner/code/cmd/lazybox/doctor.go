package main

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"golang.org/x/sys/unix"
)

// check is one line of `lazybox doctor`: what was looked at, what was found,
// and, when it is not fine, the one thing to do about it.
type check struct {
	ID     string `json:"id"`
	Title  string `json:"title"`
	Status string `json:"status"` // ok, warn, fail
	Detail string `json:"detail"`
	Fix    string `json:"fix,omitempty"`
}

const (
	checkOK   = "ok"
	checkWarn = "warn"
	checkFail = "fail"
)

func (c *cli) doctor(args []string) error {
	f := c.flags("doctor")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	checks := c.diagnose(c.ctx)
	failed := 0
	for _, k := range checks {
		if k.Status == checkFail {
			failed++
		}
	}
	if c.json {
		if err := c.printJSON(map[string]any{"version": version.String(), "checks": checks, "healthy": failed == 0}); err != nil {
			return err
		}
	} else {
		c.out("%s %s\n", c.paint(bold, "Lazybox doctor"), c.paint(dim, version.String()))
		for _, k := range checks {
			icon := map[string]string{checkOK: c.paint(green, "✓"), checkWarn: c.paint(yellow, "!"), checkFail: c.paint(red, "✗")}[k.Status]
			c.out("%s %-18s %s\n", icon, k.Title, k.Detail)
			if k.Fix != "" {
				c.out("  %-18s %s\n", "", c.paint(dim, "→ "+k.Fix))
			}
		}
		if failed == 0 {
			c.out("\nEverything Lazybox needs is in place.\n")
		} else {
			c.out("\n%d problem%s to fix. `lazybox service logs` shows what the Runner reported.\n", failed, map[bool]string{true: "", false: "s"}[failed == 1])
		}
	}
	if failed > 0 {
		return exitError{exitFailure, fmt.Errorf("%d doctor check(s) failed", failed)}
	}
	return nil
}

// diagnose runs every check. A check that cannot run reports why instead of
// stopping the others: doctor is what a person runs when things are broken.
func (c *cli) diagnose(ctx context.Context) []check {
	var checks []check
	add := func(k check) { checks = append(checks, k) }
	in, err := c.setup().installer()
	if err != nil {
		add(check{ID: "release", Title: "Release", Status: checkFail, Detail: err.Error(), Fix: platformFix(
			"Install Lazybox from its disk image or `brew install --cask lazybox-v2`, then run the `lazybox` it ships",
			"Unpack a Lazybox Runner release (docs/install.md, \"Runner on a Linux machine\"), then run the `lazybox` it ships")})
		return checks
	}
	release, _ := in.Layout.ReadVersion()
	app := in.Layout.AppBundle()
	if app == "" {
		app = in.Layout.Root
	}
	if err = in.Layout.Check(); err != nil {
		add(check{ID: "release", Title: "Release", Status: checkFail, Detail: err.Error(), Fix: platformFix("Reinstall Lazybox: drag a fresh copy to Applications", "Unpack the release again, after checking its SHA-256")})
	} else if reason := in.Layout.Unstable(); reason != "" {
		add(check{ID: "release", Title: "Release", Status: checkFail, Detail: release + " at " + app, Fix: reason})
	} else {
		add(check{ID: "release", Title: "Release", Status: checkOK, Detail: release + " at " + app})
	}

	// The service and the Runner it runs.
	report := c.serviceStatus(ctx, in)
	switch {
	case report.ManagerProblem != "":
		add(check{ID: "service", Title: "Runner service", Status: checkFail, Detail: report.ManagerProblem})
	case !report.Installed:
		add(check{ID: "service", Title: "Runner service", Status: checkFail, Detail: "not installed", Fix: platformFix("Open Lazybox and choose Set up, or run `lazybox service install`", "`lazybox service install`")})
	case !report.Running:
		add(check{ID: "service", Title: "Runner service", Status: checkFail, Detail: "installed but not running" + suffix(" (last exit "+report.LastExit+")", report.LastExit != ""), Fix: "`lazybox service start`, then `lazybox service logs` if it stops again"})
	case report.Program != in.Layout.Runner():
		add(check{ID: "service", Title: "Runner service", Status: checkWarn, Detail: "runs " + report.Program + ", not this release's Runner", Fix: "`lazybox service install` points it at this release; running agents are kept"})
	default:
		detail := fmt.Sprintf("running (pid %d) under %s", report.PID, report.Manager)
		if report.StartsWhen != "" {
			detail += ", " + report.StartsWhen
		}
		add(check{ID: "service", Title: "Runner service", Status: checkOK, Detail: detail})
	}
	if k, ok := lingerCheck(ctx, in); ok {
		add(k)
	}
	if k, ok := hostsCheck(in); ok {
		add(k)
	}
	if info, err := os.Stat(in.Paths.Endpoint()); err != nil {
		add(check{ID: "endpoint", Title: "Endpoint", Status: checkFail, Detail: "no endpoint file at " + in.Paths.Endpoint(), Fix: "The Runner writes it when it starts: `lazybox service start`"})
	} else if info.Mode().Perm()&0o077 != 0 {
		add(check{ID: "endpoint", Title: "Endpoint", Status: checkFail, Detail: fmt.Sprintf("%s is readable by other users (%v)", in.Paths.Endpoint(), info.Mode().Perm()), Fix: "chmod 600 " + in.Paths.Endpoint() + ", then `lazybox service restart` for a new credential"})
	} else if !report.Reachable {
		add(check{ID: "endpoint", Title: "Endpoint", Status: checkFail, Detail: report.Problem, Fix: "`lazybox service restart`; `lazybox service logs` shows why it stopped answering"})
	} else {
		add(check{ID: "endpoint", Title: "Endpoint", Status: checkOK, Detail: report.URL + " answers (loopback only)"})
	}
	if report.Reachable {
		switch {
		case report.RunnerVersion != release:
			add(check{ID: "versions", Title: "Versions", Status: checkFail, Detail: fmt.Sprintf("Runner %s, app and CLI %s", report.RunnerVersion, release), Fix: "`lazybox service install` restarts the Runner on this release; agents keep running"})
		case version.String() != release:
			add(check{ID: "versions", Title: "Versions", Status: checkWarn, Detail: fmt.Sprintf("this lazybox is %s, the release is %s", version.String(), release), Fix: "`lazybox link` points your PATH at this release's CLI"})
		default:
			add(check{ID: "versions", Title: "Versions", Status: checkOK, Detail: "app, CLI and Runner are all " + release})
		}
	}

	// What agents run with.
	if out, err := exec.CommandContext(ctx, in.Layout.Node(), "--version").CombinedOutput(); err != nil {
		detail := "the bundled Node does not run: " + err.Error()
		if text := strings.TrimSpace(string(out)); text != "" {
			detail += ": " + strings.SplitN(text, "\n", 2)[0]
		}
		add(check{ID: "node", Title: "Node", Status: checkFail, Detail: detail, Fix: platformFix("Reinstall Lazybox; macOS may have quarantined part of it",
			"Install the libraries Node needs: `sudo apt install libatomic1 libstdc++6` (Debian, Ubuntu) or `sudo dnf install libatomic libstdc++` (Fedora, RHEL); it also needs glibc 2.28 or newer (`ldd --version`)")})
	} else {
		add(check{ID: "node", Title: "Node", Status: checkOK, Detail: strings.TrimSpace(string(out)) + " (bundled; runs the Claude worker only)"})
	}
	if _, err := os.Stat(in.Layout.Worker()); err != nil {
		add(check{ID: "worker", Title: "Claude worker", Status: checkFail, Detail: err.Error(), Fix: "Reinstall Lazybox"})
	} else {
		add(check{ID: "worker", Title: "Claude worker", Status: checkOK, Detail: "bundled"})
	}
	servicePath := report.Path
	if servicePath == "" {
		servicePath = c.getenv("PATH")
	}
	add(c.providerCheck(ctx, "claude", "Claude Code", in.Layout.ClaudeBinary(), servicePath))
	add(c.providerCheck(ctx, "codex", "Codex", "", servicePath))
	for _, tool := range []string{"git", "gh"} {
		if p := lookIn(tool, servicePath); p != "" {
			add(check{ID: tool, Title: tool, Status: checkOK, Detail: p})
		} else if tool == "git" {
			add(check{ID: tool, Title: tool, Status: checkFail, Detail: "not on the Runner's PATH", Fix: platformFix("Install the Xcode command line tools (`xcode-select --install`)", "Install git with the system's package manager (`sudo apt install git`, `sudo dnf install git`)") + ", then `lazybox service install` to refresh the PATH"})
		} else {
			add(check{ID: tool, Title: tool, Status: checkWarn, Detail: "not on the Runner's PATH (optional: agents use it for pull requests)", Fix: platformFix("`brew install gh`", "Install the GitHub CLI (https://cli.github.com)") + ", then `lazybox service install` to refresh the PATH"})
		}
	}

	// GitHub, and which lazybox agents reach, through the Runner.
	if report.Reachable {
		if t, err := loadTarget("", in.Paths.Endpoint(), false); err == nil {
			probe, cancel := context.WithTimeout(ctx, 35*time.Second)
			v, err := dial(t).Version(probe, connect.NewRequest(&pb.VersionRequest{WaitAgentCli: true}))
			cancel()
			if err != nil {
				add(check{ID: "agent-cli", Title: "Agents' lazybox", Status: checkWarn, Detail: "could not ask the Runner: " + err.Error()})
			} else {
				add(agentCLICheck(v.Msg.AgentCli))
			}
		}
		if t, err := loadTarget("", in.Paths.Endpoint(), false); err == nil {
			probe, cancel := context.WithTimeout(ctx, 5*time.Second)
			gh, err := dial(t).GetGitHubConnection(probe, connect.NewRequest(&pb.GetGitHubConnectionRequest{}))
			cancel()
			switch {
			case err != nil:
				add(check{ID: "github", Title: "GitHub", Status: checkWarn, Detail: "could not read the connection: " + err.Error()})
			case gh.Msg.State == "connected":
				add(check{ID: "github", Title: "GitHub", Status: checkOK, Detail: "connected as " + gh.Msg.Login + suffix(" ("+gh.Msg.AuthMethod+")", gh.Msg.AuthMethod != "")})
			default:
				detail := "not connected (optional)"
				if gh.Msg.Error != "" {
					detail = gh.Msg.Error
				}
				add(check{ID: "github", Title: "GitHub", Status: checkWarn, Detail: detail, Fix: "Settings → GitHub in the app"})
			}
		}
		// Whether each agent can start work: the Runner's own provider
		// availability, the check New conversation and Ask Lazybox use.
		if t, err := loadTarget("", in.Paths.Endpoint(), false); err == nil {
			probe, cancel := context.WithTimeout(ctx, 30*time.Second)
			av, err := dial(t).GetProviderAvailability(probe, connect.NewRequest(&pb.ProviderAvailabilityRequest{}))
			cancel()
			if err != nil {
				add(check{ID: "signin", Title: "Agents' sign-in", Status: checkWarn, Detail: "could not ask the Runner: " + err.Error()})
			} else {
				for _, p := range av.Msg.Providers {
					if k, ok := signInCheck(p); ok {
						add(k)
					}
				}
			}
		}
	}

	// Room to work, and the person's own shell.
	add(diskCheck(in.Paths.Support))
	add(c.shellCheck(ctx, in))
	if k, ok := c.servicePathCheck(ctx, in, report.Path); ok {
		add(k)
	}
	add(c.pathCheck(in))
	return checks
}

// agentCLICheck says which lazybox agents' shells reach: shell startup files
// can put another one (V1's) first on PATH, which the Runner's shell
// integration undoes.
func agentCLICheck(a *pb.AgentCliCheck) check {
	k := check{ID: "agent-cli", Title: "Agents' lazybox"}
	switch {
	case a == nil:
		k.Status, k.Detail = checkWarn, "the Runner gives agents no lazybox CLI, or has not checked it yet"
	case a.Problem != "":
		k.Status, k.Detail = checkWarn, "could not check "+a.Shell+": "+a.Problem
	case len(a.Unfixed) > 0:
		k.Status, k.Detail = checkFail, strings.Join(a.Unfixed, "; ")
		k.Fix = "Stop your shell startup files putting another lazybox first on PATH, or remove it. Until then agents call " + a.Cli + " by its full path."
		if a.Shadow != "" {
			k.Fix = "Remove or rename " + a.Shadow + ", or stop your shell startup files putting its directory first on PATH. Until then agents call " + a.Cli + " by its full path."
		}
	case a.Shadow != "":
		k.Status, k.Detail = checkOK, a.Cli+" (your startup files put "+a.Shadow+" first in "+strings.Join(a.ShadowIn, ", ")+"; Lazybox puts its own back first for agents)"
	default:
		k.Status, k.Detail = checkOK, a.Cli
	}
	return k
}

// signInCheck is one line per installed Claude Code or Codex: whether a
// subscription can take work now, from the Runner's provider availability.
// ACP agents sign in their own ways and are left out, as are providers that
// are not installed (providerCheck says so).
func signInCheck(p *pb.ProviderStatus) (check, bool) {
	if p.GetAcp() != nil || !p.GetInstalled() || p.GetState() == "not_installed" {
		return check{}, false
	}
	title := map[string]string{"claude": "Claude sign-in", "codex": "Codex sign-in"}[p.GetProvider()]
	if title == "" {
		title = p.GetTitle() + " sign-in"
	}
	k := check{ID: "signin-" + p.GetProvider(), Title: title, Status: checkWarn}
	signIn := map[string]string{
		"claude": "Settings → Subscriptions → Add subscription → Claude, or `claude` then /login in a terminal",
		"codex":  "Settings → Subscriptions → Add subscription → Codex, or `codex login` in a terminal",
	}[p.GetProvider()]
	var usable, others []string
	for _, s := range p.GetSubscriptions() {
		label := firstNonEmpty(s.GetLabel(), "your CLI's sign-in")
		switch s.GetState() {
		case "available":
			usable = append(usable, label)
		case "unverified":
			usable = append(usable, label+" (not checked yet)")
		default:
			why := firstNonEmpty(s.GetDetail(), s.GetState())
			if !strings.EqualFold(why, "not signed in") {
				others = append(others, label+": "+why)
			}
		}
	}
	why := func(lead string) string {
		if len(others) == 0 {
			return lead
		}
		return lead + " (" + strings.Join(others, "; ") + ")"
	}
	switch p.GetState() {
	case "ready":
		k.Status, k.Detail = checkOK, strings.Join(usable, ", ")
	case "no_subscription", "sign_in":
		k.Detail, k.Fix = why("not signed in"), signIn
	case "limited":
		k.Detail, k.Fix = why("every subscription is at its limit"), "Wait for the reset Settings → Subscriptions shows, or add another subscription there"
	case "billing":
		k.Detail, k.Fix = why("a billing problem"), "Check the account's plan and billing with the provider"
	default:
		k.Detail = firstNonEmpty(p.GetError(), p.GetState())
	}
	return k, true
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

func (c *cli) providerCheck(ctx context.Context, id, title, bundled, servicePath string) check {
	// The Runner runs the Claude Code the SDK ships (standalone.go), so that
	// one is what matters; another provider comes from the service's PATH.
	program := ""
	if bundled != "" {
		if _, err := os.Stat(bundled); err == nil {
			program = bundled
		}
	}
	if program == "" {
		program = lookIn(id, servicePath)
	}
	if program == "" {
		fix := map[string]string{"codex": platformFix("`brew install codex` or `npm i -g @openai/codex`", "`npm i -g @openai/codex`") + ", then `lazybox service install` to refresh the PATH"}[id]
		if id == "claude" && runtime.GOOS == "linux" {
			// The Linux release does not ship Claude Code (REMOTE_RUNNERS.md §5.3).
			fix = "Install Claude Code with Anthropic's installer (https://docs.anthropic.com/en/docs/claude-code), then `lazybox service install` to refresh the PATH"
		}
		return check{ID: id, Title: title, Status: checkWarn, Detail: "not installed (optional: only needed to run " + title + " agents)", Fix: fix}
	}
	probe, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	out, err := exec.CommandContext(probe, program, "--version").Output()
	if err != nil {
		return check{ID: id, Title: title, Status: checkWarn, Detail: program + " does not run: " + err.Error()}
	}
	detail := strings.TrimSpace(strings.SplitN(string(out), "\n", 2)[0])
	if program == bundled {
		detail += " (bundled with the SDK)"
	} else {
		detail += " at " + program
	}
	return check{ID: id, Title: title, Status: checkOK, Detail: detail}
}

// lookIn finds an executable on a PATH other than this process's: the
// service's, which is what agents get.
func lookIn(name, path string) string {
	for _, dir := range filepath.SplitList(path) {
		if dir == "" {
			continue
		}
		candidate := filepath.Join(dir, name)
		if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0 {
			return candidate
		}
	}
	return ""
}

func diskCheck(dir string) check {
	for ; dir != "/" && dir != "."; dir = filepath.Dir(dir) {
		if _, err := os.Stat(dir); err == nil {
			break
		}
	}
	var fs unix.Statfs_t
	if err := unix.Statfs(dir, &fs); err != nil {
		return check{ID: "disk", Title: "Disk", Status: checkWarn, Detail: "could not read free space: " + err.Error()}
	}
	free := fs.Bavail * uint64(fs.Bsize)
	detail := humanBytes(free) + " free"
	switch {
	case free < 1<<30:
		return check{ID: "disk", Title: "Disk", Status: checkFail, Detail: detail, Fix: "Free some space: agents' worktrees, transcripts and command output need room, and a full disk stops a conversation"}
	case free < 5<<30:
		return check{ID: "disk", Title: "Disk", Status: checkWarn, Detail: detail, Fix: "Less than 5 GB left; worktrees and builds can fill it quickly"}
	}
	return check{ID: "disk", Title: "Disk", Status: checkOK, Detail: detail}
}

// shellCheck says whether the login shell's PATH could be read: the service
// install and the PATH checks below depend on it.
func (c *cli) shellCheck(ctx context.Context, in installation.Installer) check {
	_, problem := c.personPath(ctx, in)
	shell := c.shellEnv.Shell
	if shell == "" {
		shell = c.getenv("SHELL")
	}
	if problem != "" {
		return check{ID: "shell", Title: "Login shell", Status: checkWarn, Detail: shell + ": " + problem + "; agents get a standard PATH", Fix: "`" + shell + " -l -i -c true` must finish within a few seconds without asking anything; then `lazybox service install`"}
	}
	return check{ID: "shell", Title: "Login shell", Status: checkOK, Detail: shell + ": PATH read"}
}

// servicePathCheck compares the PATH the Runner gives agents (read from the
// login shell when the service was installed) with the login shell's now:
// a directory added since, such as a new version manager's, is missing for
// agents until the service is installed again.
func (c *cli) servicePathCheck(ctx context.Context, in installation.Installer, servicePath string) (check, bool) {
	path, problem := c.personPath(ctx, in)
	if problem != "" || servicePath == "" {
		return check{}, false
	}
	have := map[string]bool{}
	for _, dir := range filepath.SplitList(servicePath) {
		have[filepath.Clean(dir)] = true
	}
	var missing []string
	for _, dir := range filepath.SplitList(installation.ServicePath(path, in.Paths.Home)) {
		if !have[filepath.Clean(dir)] {
			missing = append(missing, dir)
		}
	}
	if len(missing) > 0 {
		return check{ID: "service-path", Title: "Agents' PATH", Status: checkWarn, Detail: "lacks " + strings.Join(missing, ", ") + ", which your login shell has", Fix: "`lazybox service install` reads your shell's PATH again; agents keep running"}, true
	}
	return check{ID: "service-path", Title: "Agents' PATH", Status: checkOK, Detail: "your login shell's PATH"}, true
}

// pathCheck says which lazybox a terminal runs, on the person's login-shell
// PATH: an older Lazybox from Homebrew in /opt/homebrew/bin usually comes
// before the directory Lazybox links into.
func (c *cli) pathCheck(in installation.Installer) check {
	found, _, other := c.whichRuns(in)
	switch {
	case found == "":
		return check{ID: "cli", Title: "lazybox on PATH", Status: checkWarn, Detail: "not on your PATH", Fix: "`" + in.Layout.CLI() + " link`, or Settings → About in the app"}
	case other != nil:
		return check{ID: "cli", Title: "lazybox on PATH", Status: checkWarn, Detail: other.Problem, Fix: other.Fix}
	}
	return check{ID: "cli", Title: "lazybox on PATH", Status: checkOK, Detail: found}
}

// platformFix is the fix for this platform: macOS's, or Linux's.
func platformFix(macOS, linux string) string {
	if runtime.GOOS == "linux" {
		return linux
	}
	return macOS
}

// lingerCheck says, under systemd, whether the Runner keeps running after
// the person logs out and starts at boot: only with lingering on.
func lingerCheck(ctx context.Context, in installation.Installer) (check, bool) {
	s, ok := in.Manager.(installation.Systemd)
	if !ok {
		return check{}, false
	}
	linger, err := s.Linger(ctx)
	switch {
	case err != nil:
		return check{ID: "linger", Title: "Lingering", Status: checkWarn, Detail: "could not read it: " + err.Error()}, true
	case !linger:
		return check{ID: "linger", Title: "Lingering", Status: checkWarn, Detail: "off: the Runner and its agents stop when you log out, and it does not start at boot",
			Fix: "`loginctl enable-linger " + s.User + "` (some systems need `sudo`)"}, true
	}
	return check{ID: "linger", Title: "Lingering", Status: checkOK, Detail: "on: the Runner runs without a login session and starts at boot"}, true
}

// hostsCheck says, on Linux, whether agents outlive a stop of the Runner's
// service: under systemd, only in scopes of their own.
func hostsCheck(in installation.Installer) (check, bool) {
	if runtime.GOOS != "linux" {
		return check{}, false
	}
	status := sessionhost.Scopes()
	switch {
	case status.Scoped:
		return check{ID: "hosts", Title: "Agents outlive", Status: checkOK, Detail: "a Runner restart: each agent, command and terminal runs in a systemd scope of its own"}, true
	case in.Manager.Kind() == installation.ManagerSystemd:
		return check{ID: "hosts", Title: "Agents outlive", Status: checkWarn, Detail: "a Runner restart, through the unit's KillMode=process only: agents cannot get scopes of their own (" + status.Reason + ")",
			Fix: "Make sure `systemd-run --user --scope true` works for this account"}, true
	}
	return check{ID: "hosts", Title: "Agents outlive", Status: checkOK, Detail: "a Runner restart, not a restart of this machine or container (" + status.Reason + ")"}, true
}

func humanBytes(n uint64) string {
	switch {
	case n >= 1<<30:
		return strconv.FormatFloat(float64(n)/(1<<30), 'f', 1, 64) + " GB"
	case n >= 1<<20:
		return strconv.FormatFloat(float64(n)/(1<<20), 'f', 0, 64) + " MB"
	}
	return strconv.FormatUint(n, 10) + " bytes"
}

func suffix(s string, when bool) string {
	if when {
		return s
	}
	return ""
}
