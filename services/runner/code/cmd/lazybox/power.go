package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"golang.org/x/term"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

// The power verbs: everything a person does in the app, from a script
// (POWER.md, cmd/lazybox/README.md). One table drives dispatch, usage and
// the shell completions, so they cannot drift apart.

// verb is one power verb.
type verb struct {
	Name, Args, Summary string
	// Subs are its subcommands; Flags its options (with =a|b values to complete).
	Subs  []string
	Flags []string
	// Complete names what its first argument completes to: conversations,
	// workspaces, targets (both), actions, hooks, panels, shells.
	Complete string
	// legacy reports the old flag form of a verb the test client already had
	// (`send --session X --text …`); main handles that form as before.
	legacy func(args []string) bool
	run    func(c *cli, args []string) error
}

var verbs []verb

func init() {
	verbs = []verb{
		{Name: "ls", Args: "[--all]", Summary: "workspaces, conversations and agents with live status", Flags: []string{"--json", "--all"}, run: (*cli).ls},
		{Name: "inbox", Summary: "what needs you: needs input, failed, working, ready for review", Flags: []string{"--json", "--all"}, run: (*cli).inbox},
		{Name: "runners", Summary: "the Runners this CLI can reach (Dev, Working, …)", Flags: []string{"--json"}, run: (*cli).runners},
		{Name: "send", Args: "<conversation> [text|-]", Summary: "send a message (stdin with - or when piped)", Flags: []string{"--json", "--steer"}, Complete: "conversations", legacy: legacyUnless("--session", "--text", "--image", "--id"), run: (*cli).send},
		{Name: "open", Args: "<workspace|conversation|file[:line]>", Summary: "show it in the Lazybox window", Flags: []string{"--json", "--panel=" + strings.Join(panelKinds, "|"), "--workspace"}, Complete: "targets", legacy: legacyUnless("--cwd", "--collection", "--profile", "--resume", "--model", "--permission", "--workflow", "--gateway"), run: (*cli).open},
		{Name: "workspace", Args: "repos | request-repo <name|owner/repo|path> --reason WHY", Summary: "a workspace's repositories; an agent asks the person to add one", Subs: []string{"repos", "request-repo"}, Flags: []string{"--json", "--reason", "--no-wait", "--timeout"}, Complete: "workspaces", run: (*cli).workspace},
		{Name: "repos", Summary: "repositories and the person's home checkout of each (their own clone: logins, dev servers)", Flags: []string{"--json"}, run: (*cli).repos},
		{Name: "repo", Args: "home <name|owner/repo> [--create [--path P] | --adopt PATH | --forget | --update]", Summary: "print a repository's home checkout; create, adopt, forget or update one", Subs: []string{"home"}, Flags: []string{"--json", "--create", "--path", "--adopt", "--forget", "--update"}, run: (*cli).repo},
		{Name: "review", Args: "add --file F --line N[-M] --severity S --title T [--body B] | list | resolve ID | reopen ID", Summary: "a reviewer agent files its findings in the person's Review tray, and settles them in later rounds", Subs: []string{"add", "list", "resolve", "reopen"}, Flags: []string{"--json", "--file", "--line", "--severity=blocker|major|minor|nit", "--title", "--body", "--key", "--note"}, run: (*cli).review},
		{Name: "ask", Args: "author|reviewer \"question\" [--timeout 90s] [--no-wait]", Summary: "the author and the reviewer of a review ask each other; the reply comes back", Flags: []string{"--json", "--timeout", "--no-wait"}, run: (*cli).askAgent},
		{Name: "agents", Args: "spawn|ls|send|wait|watch|stop|report …", Summary: "an orchestrator starts and drives the agents it started (with the person's grant); its agents report back", Subs: []string{"spawn", "ls", "send", "wait", "watch", "stop", "report"}, Flags: []string{"--json", "--session", "--title", "--brief", "--task", "--workspace=same|new", "--permission=read-only|ask|yolo", "--profile", "--model", "--role", "--key", "--until=settled|idle|needs-input|reported|closed", "--any", "--timeout", "--end", "--reason"}, run: (*cli).agents},
		{Name: "conversation", Args: "new --title T (--brief B|-|--brief-artifact ID) [--workspace same|new] [--provider P] [--model M]", Summary: "an agent asks the person to start a conversation; they approve it in one click", Subs: []string{"new"}, Flags: []string{"--json", "--session", "--title", "--brief", "--brief-artifact", "--workspace=same|new", "--provider", "--profile", "--model", "--reason", "--timeout", "--no-wait"}, run: (*cli).conversation},
		{Name: "mission", Args: "plan <file|-> | approve|reject <mission> [--plan ID] [--cap $] [--wip N] [--note …] | budget <mission> --cap $", Summary: "an orchestrator proposes its plan; the person approves it with a $ cap, rejects it, or raises the budget", Subs: []string{"plan", "approve", "reject", "budget"}, Flags: []string{"--json", "--session", "--key", "--plan", "--cap", "--wip", "--note"}, run: (*cli).mission},
		{Name: "lease", Args: "claim <path> [--exclusive] [--ttl 2h] [--task ID] | release <id|path> | ls", Summary: "an agent's advisory lease on the paths its task changes; overlaps are recorded, nothing is locked", Subs: []string{"claim", "release", "ls"}, Flags: []string{"--json", "--session", "--exclusive", "--ttl", "--task", "--reason"}, run: (*cli).lease},
		{Name: "new", Args: "<workspace-name> --repo R [--repo R…] [--branch B] | --from <url|owner/name#N> [MESSAGE]", Summary: "create a workspace from catalog repositories, or start one from an issue", Flags: []string{"--json", "--repo", "--branch", "--from", "--name", "--profile", "--permission=ask|read-only|yolo", "--another", "--no-comments"}, run: (*cli).newWorkspace},
		{Name: "approve", Args: "<request|conversation>", Summary: "allow a pending approval", Flags: []string{"--json"}, Complete: "conversations", legacy: legacyUnless("--request", "--decision", "--session", "--id"), run: func(c *cli, args []string) error { return c.decide(args, true) }},
		{Name: "deny", Args: "<request|conversation>", Summary: "decline a pending approval", Flags: []string{"--json"}, Complete: "conversations", run: func(c *cli, args []string) error { return c.decide(args, false) }},
		{Name: "stop", Args: "<conversation|command> [--agent ID] [--end]", Summary: "interrupt a turn, stop a sub-agent or a command, or end a conversation", Flags: []string{"--json", "--agent", "--end"}, Complete: "conversations", run: (*cli).stop},
		{Name: "wait", Args: "<conversation> --until idle|needs-input [--timeout 30m]", Summary: "block until the conversation is idle or needs you", Flags: []string{"--json", "--until=idle|needs-input", "--timeout"}, Complete: "conversations", legacy: legacyUnless("--id"), run: (*cli).wait},
		{Name: "watch", Args: "<conversation|workspace> [--since CURSOR|DURATION]", Summary: "follow a conversation's events live, as a timeline", Flags: []string{"--json", "--since", "--tail"}, Complete: "targets", run: (*cli).watch},
		{Name: "debug", Args: "<conversation> [--events N]", Summary: "diagnose a conversation: state, recent lifecycle, findings", Flags: []string{"--json", "--events"}, Complete: "conversations", run: (*cli).debug},
		{Name: "missions", Args: "[ls|show ID|new TITLE|accept ID TASK|reject ID TASK]", Summary: "missions and their tasks; accept or reject work", Subs: []string{"ls", "show", "new", "accept", "reject"}, Flags: []string{"--json", "--brief", "--note"}, run: (*cli).missions},
		{Name: "shot", Args: "[window|PANEL] [--out FILE]", Summary: "PNG of the Lazybox window or one panel (desktop app)", Flags: []string{"--json", "--out"}, Complete: "panels", run: (*cli).shot},
		{Name: "actions", Args: "[ls|validate [FILE]|schema|path|init|run ID]", Summary: "your actions file: list, check, create, run an action", Subs: []string{"ls", "validate", "schema", "path", "init", "run"}, Flags: []string{"--json", "--conversation", "--timeout"}, Complete: "actions", run: (*cli).actions},
		{Name: "do", Args: "<action> [argument] | --list", Summary: "run any action of the app in the open window (lazybox do --list)", Flags: []string{"--json", "--list", "--workspace", "--conversation", "--timeout"}, Complete: "registry", run: (*cli).do},
		{Name: "config", Args: "[path|edit|validate|get|set|export|import] [FILE] …", Summary: "your config files: keys, settings, layouts, actions (comments kept)", Subs: []string{"path", "edit", "validate", "get", "set", "export", "import"}, Flags: []string{"--json", "--out", "--actions", "--dry-run", "--remove"}, Complete: "configfiles", run: (*cli).config},
		{Name: "extension", Args: "new <id> [--example E] | validate [<id>|PATH…] | list | path", Summary: "sandboxed panel extensions: scaffold, check, list (approve them in Settings)", Subs: []string{"new", "validate", "list", "path"}, Flags: []string{"--json", "--name", "--example=agent-board|cost-dashboard"}, run: (*cli).extension},
		{Name: "hooks", Args: "[ls|runs|test ID|log RUN]", Summary: "your hooks, their recent runs and output", Subs: []string{"ls", "runs", "test", "log"}, Flags: []string{"--json", "--limit"}, Complete: "hooks", run: (*cli).hooks},
		{Name: "completion", Args: "zsh|bash|fish", Summary: "print a shell completion script", Subs: []string{"zsh", "bash", "fish"}, run: (*cli).completion},
		// An installed release (INSTALL.md): its Runner service, the CLI on PATH, a health check, removal.
		{Name: "service", Args: "[status|install|uninstall|start|stop|restart|logs [-f]|plan]", Summary: "the Runner service: install, update, start, stop, its logs", Subs: []string{"status", "install", "uninstall", "start", "stop", "restart", "logs", "plan"}, Flags: []string{"--json", "--no-start", "--follow", "--lines"}, run: (*cli).service},
		{Name: "doctor", Summary: "check the service, endpoint, versions, Node, providers, GitHub and disk", Flags: []string{"--json"}, run: (*cli).doctor},
		{Name: "link", Args: "[--dir DIR] [--shell zsh|bash|fish] | --remove-older [--yes] | <pull request URL> [--role delivers|mentions]", Summary: "put this lazybox on your PATH and install shell completions (no sudo); with a URL, track a pull request this workspace delivers", Flags: []string{"--json", "--dir", "--shell=zsh|bash|fish", "--no-completions", "--force", "--dry-run", "--remove-older", "--yes", "--role=delivers|mentions", "--workspace"}, run: (*cli).link},
		{Name: "items", Args: "[--workspace W | --all]", Summary: "the pull requests a workspace delivers, their checks and reviews, and what it mentions", Flags: []string{"--json", "--workspace", "--all"}, run: (*cli).items},
		{Name: "uninstall", Args: "[--dry-run] [--remove-data [--yes]]", Summary: "remove the service, links and completions (and, if asked, your data)", Flags: []string{"--json", "--dry-run", "--remove-data", "--yes"}, run: (*cli).uninstall},
		{Name: "version", Summary: "this lazybox's version (also --version)", Flags: []string{"--json"}, run: (*cli).version},
		{Name: "schema", Args: "[actions|process|workspace|keys|settings|layout|theme] [--write DIR]", Summary: "the JSON Schemas of the files you write, for your editor (no web address)", Subs: []string{"actions", "process", "workspace", "keys", "settings", "layout", "theme"}, Flags: []string{"--json", "--write"}, run: (*cli).schema},
		{Name: "__complete", Summary: "", run: (*cli).complete},
	}
}

// panelKinds are the panels `open --panel` and `shot` name (the app's panel kinds).
var panelKinds = []string{"chat", "console", "terminal", "assistant", "changes", "usage", "sessions", "missions", "pulls", "processes", "actions", "preview", "files", "editor", "artifacts", "settings", "inbox"}

// legacyUnless marks the old flag form: any of these flags, or no positional argument.
func legacyUnless(flags ...string) func([]string) bool {
	return func(args []string) bool {
		positional := false
		for _, a := range args {
			name, _, _ := strings.Cut(a, "=")
			if slices.Contains(flags, name) || slices.Contains(flags, "-"+strings.TrimLeft(name, "-")) || slices.Contains(flags, "--"+strings.TrimLeft(name, "-")) {
				return true
			}
			if !strings.HasPrefix(a, "-") {
				positional = true
			}
		}
		return !positional
	}
}

func findVerb(name string) *verb {
	for i := range verbs {
		if verbs[i].Name == name {
			return &verbs[i]
		}
	}
	return nil
}

// cli is one invocation of a power verb.
type cli struct {
	ctx            context.Context
	stdin          io.Reader
	stdout, stderr io.Writer
	getenv         func(string) string
	// endpoint and runner are the global --endpoint and --runner.
	endpoint, runner string
	stdinTTY, color  bool
	now              func() time.Time
	// poll is how often wait and watch look again.
	poll time.Duration
	// json: the verb's --json.
	json bool
	// install finds the release and the system around it (install.go).
	install setup
	// shellEnv is the login shell's environment, read once (personPath).
	shellEnv *installation.ShellEnvironment
	// last is the client the verb dialed, to explain its failure.
	last *client
}

func newCLI(ctx context.Context, endpointFlag, runnerFlag string) *cli {
	return &cli{ctx: ctx, stdin: os.Stdin, stdout: os.Stdout, stderr: os.Stderr, getenv: os.Getenv, endpoint: endpointFlag, runner: runnerFlag,
		stdinTTY: term.IsTerminal(int(os.Stdin.Fd())), color: term.IsTerminal(int(os.Stdout.Fd())) && os.Getenv("NO_COLOR") == "" && os.Getenv("TERM") != "dumb",
		now: time.Now, poll: 500 * time.Millisecond}
}

// powerVerb runs args when they name a power verb in its new form; handled is
// false for anything main still handles.
func powerVerb(c *cli, args []string) (handled bool, err error) {
	if len(args) == 0 {
		return false, nil
	}
	v := findVerb(args[0])
	if v == nil || (v.legacy != nil && v.legacy(args[1:])) {
		return false, nil
	}
	err = v.run(c, args[1:])
	if err != nil && c.last != nil {
		err = c.last.explain(err)
	} else if err != nil {
		err = (&client{}).explain(err)
	}
	return true, err
}

// flags is a verb's flag set, with the options every power verb shares.
func (c *cli) flags(name string) *flag.FlagSet {
	f := flag.NewFlagSet("lazybox "+name, flag.ContinueOnError)
	f.SetOutput(c.stderr)
	f.BoolVar(&c.json, "json", false, "print JSON")
	f.StringVar(&c.runner, "runner", c.runner, "registered Runner to use (lazybox runners)")
	f.StringVar(&c.endpoint, "endpoint", c.endpoint, "Runner endpoint file")
	f.Usage = func() {
		if v := findVerb(name); v != nil {
			fmt.Fprintf(c.stderr, "Usage: lazybox %s %s\n  %s\n", name, v.Args, v.Summary)
		}
		f.PrintDefaults()
	}
	return f
}

// parse parses interleaved flags and positional arguments; a bad flag is a usage error.
func (c *cli) parse(f *flag.FlagSet, args []string) ([]string, error) {
	positional, err := parseInterleaved(f, args)
	if err == flag.ErrHelp {
		return nil, exitError{exitOK, err}
	}
	if err != nil {
		return nil, exitError{exitUsage, err}
	}
	return positional, nil
}

// dial connects to the resolved Runner.
func (c *cli) dial() (*client, error) {
	t, err := resolveTarget(c.getenv, c.endpoint, c.runner)
	if err != nil {
		return nil, err
	}
	c.last = dial(t)
	return c.last, nil
}

func (c *cli) out(format string, args ...any) { fmt.Fprintf(c.stdout, format, args...) }
func (c *cli) note(format string, args ...any) {
	fmt.Fprintf(c.stderr, format, args...)
}

// printJSON writes v as indented JSON.
func (c *cli) printJSON(v any) error {
	data, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	_, err = fmt.Fprintln(c.stdout, string(data))
	return err
}

// printProtoJSON writes a message as the Runner's JSON.
func (c *cli) printProtoJSON(m proto.Message) error {
	data, err := protojson.MarshalOptions{Multiline: true, Indent: "  "}.Marshal(m)
	if err != nil {
		return err
	}
	_, err = fmt.Fprintln(c.stdout, string(data))
	return err
}

// paint colours s when stdout is a terminal that wants colour.
func (c *cli) paint(code, s string) string {
	if !c.color || code == "" {
		return s
	}
	return "\x1b[" + code + "m" + s + "\x1b[0m"
}

const (
	red, green, yellow, blue, magenta, cyan, dim, bold = "31", "32", "33", "34", "35", "36", "2", "1"
)

func stateColour(s string) string {
	return map[string]string{stateNeedsInput: yellow, stateFailed: red, stateWorking: cyan, stateReview: green, stateIdle: dim, stateEnded: dim}[s]
}

func stateLabel(s string) string {
	return map[string]string{stateNeedsInput: "needs input", stateFailed: "failed", stateWorking: "working", stateReview: "review", stateIdle: "idle", stateEnded: "ended"}[s]
}

func stateIcon(s string) string {
	return map[string]string{stateNeedsInput: "●", stateFailed: "✗", stateWorking: "◐", stateReview: "✓", stateIdle: "○", stateEnded: "·"}[s]
}

// powerUsage lists the power verbs for `lazybox` with no arguments.
func powerUsage(w io.Writer) {
	fmt.Fprintln(w, "Power verbs (every one takes --json; --runner NAME picks a Runner):")
	for _, v := range verbs {
		if v.Summary == "" {
			continue
		}
		if len([]rune(v.Args)) > 44 {
			fmt.Fprintf(w, "  %-11s %s\n  %-11s %-44s %s\n", v.Name, v.Args, "", "", v.Summary)
			continue
		}
		fmt.Fprintf(w, "  %-11s %-44s %s\n", v.Name, v.Args, v.Summary)
	}
	fmt.Fprintln(w, "Exit codes: 0 ok · 1 failed · 2 usage · 3 not found · 4 not allowed · 5 timed out · 6 Runner or window unavailable.")
}
