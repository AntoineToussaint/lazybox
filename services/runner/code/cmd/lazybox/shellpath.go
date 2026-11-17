package main

import (
	"bufio"
	"bytes"
	"context"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"syscall"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
)

// legacyAliasName is a second name for the CLI that `lazybox link` made in
// earlier builds, beside `lazybox`, for when an older Lazybox shadowed it.
// There is one command now; link and uninstall remove a recorded one.
const legacyAliasName = "lazybox-v2"

// personPath is the PATH a new terminal of this person gets: their login
// shell's, read the way the service install reads it. It is what `lazybox`
// resolves against when they type it. This process's own PATH is the wrong
// one to ask when the app runs `lazybox link` or `lazybox doctor`: an app
// opened from the Finder has launchd's /usr/bin:/bin:/usr/sbin:/sbin, without
// Homebrew's directories, which hides an older Lazybox in /opt/homebrew/bin
// and makes every link directory look off PATH. When the shell cannot be
// read, this process's PATH stands in and problem says why. Read once per
// command.
func (c *cli) personPath(ctx context.Context, in installation.Installer) (path, problem string) {
	if c.shellEnv == nil {
		env := installation.ShellEnvironment{Problem: "no login shell reader"}
		if in.Shell != nil {
			env = in.Shell(ctx)
		}
		c.shellEnv = &env
	}
	if c.shellEnv.Problem != "" || c.shellEnv.Path == "" {
		problem = c.shellEnv.Problem
		if problem == "" {
			problem = "the login shell set no PATH"
		}
		return c.getenv("PATH"), problem
	}
	return c.shellEnv.Path, ""
}

// runsOn finds the first executable named name on path, as a shell would,
// and whether it is this release's CLI (the file itself or a link to it).
func runsOn(name, path, cli string) (found string, ours bool) {
	found = lookIn(name, path)
	return found, found != "" && sameFile(found, cli)
}

func sameFile(a, b string) bool {
	x, err := os.Stat(a)
	if err != nil {
		return false
	}
	y, err := os.Stat(b)
	return err == nil && os.SameFile(x, y)
}

// shadowedBefore is the `lazybox` a shell finds on path before dir, when it
// is not this release's CLI: linking into dir would not make `lazybox` run
// this release. "" when dir comes first or nothing else is found before it.
func shadowedBefore(path, dir, cli string) string {
	for _, entry := range filepath.SplitList(path) {
		if entry == "" {
			continue
		}
		if filepath.Clean(entry) == filepath.Clean(dir) {
			return ""
		}
		if found, ours := runsOn("lazybox", entry, cli); found != "" && !ours {
			return found
		}
	}
	return ""
}

// The kinds of other `lazybox` a terminal can find first.
const (
	// shadowHomebrew is an older Lazybox installed by Homebrew's `lazybox`
	// formula: the one Lazybox offers to remove, with `brew uninstall lazybox`.
	shadowHomebrew = "homebrew"
	// shadowOlder is an older Lazybox installed some other way (cargo install).
	shadowOlder = "older"
	// shadowCopy is another copy of the Lazybox app, or another build.
	shadowCopy = "copy"
	// shadowShim is a version manager's shim (mise, asdf…): not a program to
	// remove, only an order to fix.
	shadowShim = "shim"
	// shadowOther is a program Lazybox does not recognise.
	shadowOther = "other"
)

// shadow is another `lazybox` that a terminal runs instead of this release's:
// what it is, said plainly, and how to make `lazybox` run this one.
type shadow struct {
	Path string `json:"path"`
	Kind string `json:"kind"`
	// What names it: "an older Lazybox, installed by Homebrew".
	What string `json:"what"`
	// Problem and Fix are the sentences setup, link and doctor show.
	Problem string `json:"problem"`
	Fix     string `json:"fix"`
	// Remove is, for an older Lazybox from Homebrew only, the command that
	// removes it, exactly as `lazybox link --remove-older` runs it, and
	// RemoveCommand the same as a person types it.
	Remove        []string `json:"remove,omitempty"`
	RemoveCommand string   `json:"removeCommand,omitempty"`
}

// versionProbeTimeout bounds asking an unknown `lazybox` for its version.
var versionProbeTimeout = 3 * time.Second

// olderVersion is what an older Lazybox prints for --version
// ("lazybox 0.1.21"); this release adds its name in parentheses.
var olderVersion = regexp.MustCompile(`^lazybox v?\d+\.\d+\.\d+\S*$`)

// identifyShadow says what found is and how to get past it. cli is this
// release's CLI, dir is where its `lazybox` is linked (or would be) and
// onPath whether dir is on the person's PATH. found is identified by where it lives first (a
// Homebrew Cellar, an app bundle, a version manager's shims), and only then,
// when its path does not say, by asking it for --version, bounded in time,
// with no input and in its own process group.
func identifyShadow(ctx context.Context, found, cli, dir string, onPath bool) shadow {
	s := shadow{Path: found}
	resolved, err := filepath.EvalSymlinks(found)
	if err != nil {
		resolved = found
	}
	foundDir := filepath.Dir(found)
	order := "put " + dir + " before " + foundDir + " in your shell's PATH"
	if !onPath {
		order = "add " + dir + " to your shell's PATH, before " + foundDir
	}
	switch {
	case homebrewPrefix(resolved) != "":
		s.Kind, s.What = shadowHomebrew, "an older Lazybox, installed by Homebrew"
		s.RemoveCommand = "brew uninstall lazybox"
		if brew := filepath.Join(homebrewPrefix(resolved), "bin", "brew"); isExecutable(brew) {
			s.Remove = []string{brew, "uninstall", "lazybox"}
		}
		s.Fix = "`brew uninstall lazybox` (removes the older Lazybox), or " + order + "."
	case strings.HasSuffix(resolved, "/Contents/Resources/runtime/bin/lazybox"):
		s.Kind, s.What = shadowCopy, "another copy of Lazybox, in "+appOf(resolved)
		s.Fix = "`" + cli + " link --dir " + foundDir + "` points it at this copy, or " + order + "."
	case shimOf(found, resolved) != "":
		article := "a "
		if strings.ContainsAny(shimOf(found, resolved)[:1], "aeiou") {
			article = "an "
		}
		s.Kind, s.What = shadowShim, article+shimOf(found, resolved)+" shim, not Lazybox itself"
		s.Fix = capitalize(order) + ", after the line that sets up " + shimOf(found, resolved) + "; or stop " + shimOf(found, resolved) + " providing a `lazybox`."
	default:
		line := askVersion(ctx, found)
		switch {
		case strings.Contains(line, "(Lazybox"):
			s.Kind, s.What = shadowCopy, "another build of Lazybox ("+line+")"
			s.Fix = "Remove " + found + ", or " + order + "."
		case olderVersion.MatchString(line):
			s.Kind, s.What = shadowOlder, "an older Lazybox, version "+strings.TrimPrefix(strings.TrimPrefix(line, "lazybox "), "v")
			s.Fix = "Remove " + found + " (the older Lazybox), or " + order + "."
		default:
			s.Kind, s.What = shadowOther, "a program that is not this Lazybox"
			s.Fix = capitalize(order) + ", or rename or remove " + found + "."
		}
	}
	s.Problem = "Another `lazybox` (" + s.What + ", at " + found + ") comes first on your PATH, so `lazybox` runs it."
	return s
}

// homebrewPrefix is the Homebrew prefix whose `lazybox` formula installed
// resolved (/opt/homebrew for /opt/homebrew/Cellar/lazybox/0.1.21/bin/lazybox).
func homebrewPrefix(resolved string) string {
	i := strings.Index(resolved, "/Cellar/lazybox/")
	if i <= 0 {
		return ""
	}
	return resolved[:i]
}

// shimPrograms are what a version manager's shim resolves to.
var shimPrograms = []string{"mise", "rtx", "asdf", "volta-shim", "proto-shim"}

// shimOf names the version manager whose shim found is (a file in a shims
// directory, or a link to the manager itself), or "" when it is not a shim.
func shimOf(found, resolved string) string {
	base := filepath.Base(resolved)
	if filepath.Base(filepath.Dir(found)) != "shims" && !slices.Contains(shimPrograms, base) {
		return ""
	}
	for _, manager := range []string{"mise", "rtx", "asdf", "volta", "proto"} {
		if base == manager || base == manager+"-shim" || strings.Contains(found, "/"+manager+"/") || strings.Contains(found, "/."+manager+"/") {
			return manager
		}
	}
	return "version manager"
}

// askVersion is the first line program prints for --version, or "" when it
// fails or takes longer than versionProbeTimeout. It runs with no input and
// in its own process group, which is killed whole on the deadline.
func askVersion(ctx context.Context, program string) string {
	probe, cancel := context.WithTimeout(ctx, versionProbeTimeout)
	defer cancel()
	cmd := exec.CommandContext(probe, program, "--version")
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = time.Second
	out, err := cmd.Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(strings.SplitN(string(out), "\n", 2)[0])
}

func isExecutable(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0
}

// appOf is the .app a runtime CLI belongs to.
func appOf(cli string) string {
	for dir := filepath.Dir(cli); dir != "/" && dir != "."; dir = filepath.Dir(dir) {
		if strings.HasSuffix(dir, ".app") {
			return dir
		}
	}
	return cli
}

// linkedDir is where this release's `lazybox` is linked: the directory of a
// link `lazybox link` recorded that still runs it, else where it would link.
func linkedDir(in installation.Installer) string {
	for _, l := range readRecord(in.Paths).Links {
		if filepath.Base(l) == "lazybox" && sameFile(l, in.Layout.CLI()) {
			return filepath.Dir(l)
		}
	}
	return linkDirectory(in.Paths.Home)
}

func onPathList(path, dir string) bool {
	return slices.ContainsFunc(filepath.SplitList(path), func(entry string) bool {
		return entry != "" && filepath.Clean(entry) == filepath.Clean(dir)
	})
}

// whichRuns is the `lazybox` a terminal of this person runs, whether it is
// this release's, and, when another one comes first, what it is and the fix.
func (c *cli) whichRuns(in installation.Installer) (found string, ours bool, other *shadow) {
	path, _ := c.personPath(c.ctx, in)
	cli := in.Layout.CLI()
	found, ours = runsOn("lazybox", path, cli)
	if found == "" || ours {
		return found, ours, nil
	}
	dir := linkedDir(in)
	s := identifyShadow(c.ctx, found, cli, dir, onPathList(path, dir))
	return found, false, &s
}

// removeOlderResult is `lazybox link --remove-older --json`.
type removeOlderResult struct {
	// Command is what ran as a person types it; Ran is exactly what ran.
	Command string   `json:"command,omitempty"`
	Ran     []string `json:"ran,omitempty"`
	Output  string   `json:"output,omitempty"`
	Error   string   `json:"error,omitempty"`
	Removed bool     `json:"removed"`
	DryRun  bool     `json:"dryRun,omitempty"`
	// Runs is the `lazybox` a terminal runs afterwards, Ours whether it is
	// this release's, and Shadow what still comes first, if anything.
	Runs   string  `json:"runs,omitempty"`
	Ours   bool    `json:"ours"`
	Shadow *shadow `json:"shadow,omitempty"`
}

// removeOlder removes an older Lazybox installed by Homebrew that comes
// first on the person's PATH, with `brew uninstall lazybox`, after asking
// (or --yes, which the setup window passes once the person confirmed the
// command it showed). It acts only on what it finds itself, never on a
// path it is given, and only on Homebrew's `lazybox` formula: anything else
// (a shim, another program) gets its fix, not a removal.
func (c *cli) removeOlder(in installation.Installer, yes, dryRun bool) error {
	found, ours, s := c.whichRuns(in)
	if s == nil {
		res := removeOlderResult{Runs: found, Ours: ours}
		if c.json {
			return c.printJSON(res)
		}
		if ours {
			c.out("Nothing to remove: `lazybox` runs this Lazybox (%s).\n", found)
		} else {
			c.out("Nothing to remove: there is no other `lazybox` on your PATH.\n")
		}
		return nil
	}
	if s.Kind != shadowHomebrew || len(s.Remove) == 0 {
		why := "it is not an older Lazybox installed by Homebrew"
		if s.Kind == shadowHomebrew {
			why = "Homebrew's brew is not where its Cellar says"
		}
		return fail(exitFailure, "%s Lazybox removes nothing here: %s. Fix: %s", s.Problem, why, s.Fix)
	}
	command := strings.Join(s.Remove, " ")
	res := removeOlderResult{Command: s.RemoveCommand, Ran: s.Remove, DryRun: dryRun, Runs: found, Shadow: s}
	if dryRun {
		if c.json {
			return c.printJSON(res)
		}
		c.out("%s\nWould run: %s\n", s.Problem, command)
		return nil
	}
	if !yes {
		if !c.stdinTTY {
			return fail(exitUsage, "this runs `%s`; rerun with --yes to confirm, or from a terminal to be asked", command)
		}
		c.out("%s\nRun %s to remove it? [y/N] ", s.Problem, c.paint(bold, command))
		answer, _ := bufio.NewReader(c.stdin).ReadString('\n')
		if a := strings.ToLower(strings.TrimSpace(answer)); a != "y" && a != "yes" {
			return fail(exitFailure, "nothing was removed")
		}
	}
	path, _ := c.personPath(c.ctx, in)
	ctx, cancel := context.WithTimeout(c.ctx, 5*time.Minute)
	defer cancel()
	cmd := exec.CommandContext(ctx, s.Remove[0], s.Remove[1:]...)
	cmd.Env = append(os.Environ(), "PATH="+path, "HOMEBREW_NO_AUTO_UPDATE=1", "HOMEBREW_NO_ENV_HINTS=1")
	var output bytes.Buffer
	w := io.Writer(&output)
	if !c.json {
		c.out("$ %s\n", command)
		w = io.MultiWriter(&output, c.stdout)
	}
	cmd.Stdout, cmd.Stderr = w, w
	runErr := cmd.Run()
	res.Output = output.String()
	res.Removed = runErr == nil && !isExecutable(found)
	res.Runs, res.Ours, res.Shadow = c.whichRuns(in)
	if runErr != nil {
		res.Error = "`" + s.RemoveCommand + "` failed: " + runErr.Error()
	}
	if c.json {
		if err := c.printJSON(res); err != nil {
			return err
		}
	} else {
		switch {
		case res.Ours:
			c.out("%s `lazybox` now runs this Lazybox (%s).\n", c.paint(green, "✓"), res.Runs)
		case res.Shadow != nil:
			c.out("%s %s Fix: %s\n", c.paint(yellow, "!"), res.Shadow.Problem, res.Shadow.Fix)
		case res.Removed:
			c.out("%s Removed the older Lazybox. `%s link` puts this one on your PATH.\n", c.paint(green, "✓"), in.Layout.CLI())
		}
	}
	if runErr != nil {
		return fail(exitFailure, "%s", res.Error)
	}
	return nil
}
