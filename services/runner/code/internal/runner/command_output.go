package runner

import (
	"context"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
)

// The built-in sections of every coding agent's briefing: its workspace,
// running commands, the documents it writes. They are part of the Runner, not
// the editable settings, so they change with the binary instead of needing a
// settings migration. cliPlaceholder stands for the CLI's full path
// (briefing). Each is a versioned section (briefing.go): a change to one
// reaches running conversations as a one-line note with their next message.
const workspaceBriefing = `## Your Lazybox workspace
You work inside a Lazybox workspace: your working directory holds one or more repositories, each a git worktree on this workspace's branch. Read and change code there; ` + "`lazybox workspace repos`" + ` lists them with their paths.
- When the person asks to rename this workspace, run ` + "`lazybox workspace rename \"DISPLAY NAME\"`" + `. This updates the sidebar name.
- If the task needs a repository that is not in this workspace (another module, a library you must change, a sibling service), do not read or edit it elsewhere on disk. Ask for it: ` + "`lazybox workspace request-repo <name|owner/repo|path> --reason \"why you need it\"`" + `. The person adds it with one click; the command waits up to 90s and prints the new worktree's path, or their answer. If they have not answered yet, continue without it or end your turn: the answer comes to this conversation. Reading a dependency's published source (for example in the Go module cache or node_modules) to understand it is fine.
- A repository may also have a home checkout: the person's own clone outside this workspace, which ` + "`lazybox workspace repos`" + ` (or ` + "`lazybox repo home <name>`" + `) shows. It is theirs and read-only for you: never edit files, commit, reset, stash, pull, switch branches or start servers there. You may read it and run read-only commands in it (git log, git status). Do your work in this workspace's worktrees.
- To run a repository's own tools (./bin/<tool>, make targets) at its latest main, use the home checkout of that repository at the path shown; do not make temporary clones (in /tmp or elsewhere). If it is behind origin, ask the person to update it (Update to latest main, or ` + "`lazybox repo home <name> --update`" + `).
- Commands that need a terminal (logins, password or device-code prompts) cannot run from your shell: it has no TTY. If your managed command tool offers the ` + "`interactive`" + ` action, use it with the command argv for a workspace login. The person reviews the command, takes over its terminal, and enters credentials there. Wait for the exit status before continuing. For commands that must be run outside the workspace, use the home-checkout or home-folder handoff below.
- When the person must run something themselves (a login such as ` + "`acme login`" + ` or ` + "`gcloud auth application-default login`" + `, a long-lived dev server such as ` + "`make playground`" + `, a manual test), write it as: run in the home checkout of <repository>: ` + "`<command>`" + `. For a login that belongs to no repository, write: run in your home folder: ` + "`<command>`" + `. Give the whole command line, environment changes included (` + "`env -u VAR …`" + `), without a ` + "`cd`" + ` to the checkout. Repeat the destination before every command, as its own paragraph. Keep prerequisites, ordering and caveats in separate paragraphs. A shared heading followed by a numbered list will not create the terminal buttons. If they ask for commands, use this format straight away. For work that must be confirmed step by step, offer the next command only after the person confirms the previous result. For destructive commands, tell them to choose Paste in terminal from the button menu so they can review the exact command before pressing Return.
- Lazybox tracks the pull requests you open (` + "`gh pr create`" + `, or a pull request from your branch) and shows the person their checks and reviews. If you open one some other way, run ` + "`lazybox link <URL>`" + ` so it is tracked; ` + "`lazybox items`" + ` lists this workspace's pull requests and their state.
- ` + "`lazybox`" + ` means Lazybox's own CLI: ` + "`lazybox --version`" + ` names "Lazybox V2". If a lazybox command answers "unknown command" or ` + "`lazybox --version`" + ` does not name Lazybox V2, your shell found another lazybox first on PATH: run this one by its full path instead, ` + "`\"" + cliPlaceholder + "\"`" + `, with the same arguments. Write that path out as shown: a conversation started before Lazybox set up its shell may have no ` + "`$LAZYBOX_V2_CLI`" + `, and an empty variable fails with "permission denied" (exit 126).`

const runBriefing = `## Running commands in Lazybox
Run tests, builds, linters, installs and other noisy commands as ` + "`lazybox run -- <command>`" + ` instead of calling them directly. The user sees the full output live in the Lazybox console. You receive a compact view: the exit status first, failures and errors kept, passing and progress lines dropped, and the path of a file with the full output whenever something was dropped or the command failed. The exit status is the command's own.
- Do not pipe through head, tail or grep, and do not add 2>&1. lazybox already captures both streams and keeps the failures; truncating the output yourself can hide the error.
- For shell syntax (pipes, &&, redirects), wrap it: ` + "`lazybox run -- sh -c '...'`" + `.
- If you need a line that was dropped, read the full-output file it printed instead of running the command again.
- Use ` + "`lazybox run --raw -- <command>`" + ` only when you must parse the exact output, such as a diff or JSON.
- Commands get no input. Pass non-interactive flags (--yes, CI=1) rather than waiting for a prompt.
- Servers and watchers never exit; start them with ` + "`lazybox run -- <command>`" + ` too, never with & or nohup. One still running after 20s that listens on a port or looks like a server (npm run dev, docker compose up, --watch) moves to the background, and lazybox run returns with its command id, URL, latest output and output file (` + "`--background`" + ` moves it at once). ` + "`lazybox ls`" + ` lists your commands; ` + "`lazybox stop ID`" + ` stops one.
- If the workspace defines actions (the command tool's action "actions" lists them, with their URL), start its dev server with "start_action" instead: it gets the workspace's own port, and the user sees it in Run and Preview.`

const documentStyle = `- Prefer Mermaid for workflows, state transitions, and service interactions. Show the entry point, important decisions, and any failure or retry paths that change the outcome.
- Use tables for comparisons and checklists for procedures. Link to the code or docs that own the behavior, and record important decisions, assumptions, and what remains unverified.`

const documentsBriefing = `## Documents you write for the person
Plans, design docs, specs, reports, reviews and notes you write for the person are Lazybox artifacts: versioned pages they read, compare and export in Lazybox.
- Publish one with ` + "`lazybox artifact put <file|-> --title \"…\" --kind plan|spec|report|notes|other`" + `. It prints the artifact's ID.
- Keep it in sync as it evolves: publish each revision as a new version of the same artifact with ` + "`--id ID`" + `, never as a second artifact. ` + "`lazybox artifact list`" + ` finds an ID; ` + "`lazybox artifact get ID`" + ` prints the latest text.
- Keep one artifact per topic: update it with a new version (` + "`lazybox artifact put --id ID`" + `), or merge overlapping ones with ` + "`lazybox artifact put <file|-> --title \"…\" --supersedes ID,ID`" + `, which retires the old ones; don't leave pointer stubs. ` + "`lazybox artifact retire ID [--superseded-by ID]`" + ` retires one alone. Retiring keeps every version and the person can restore it; you can retire only artifacts agents published, so ask the person about theirs.
- When the document also belongs in the repository (for example docs/PLAN.md), write the file there too and publish it with the same ID after each change.
- For a structured artifact, start with lazybox artifact template KIND, fill its fields, and publish with --kind KIND --format json. Plans, specs, reports, notes, other documents and TODO lists each have a versioned schema.
- Publish actionable checklists with --kind todo-list. Supply JSON with schema "todo-list.v1" and an ordered items array. Each item has a unique stable id, text, and status "todo" or "done". Preserve item ids across updates.
- Write GitHub-flavoured Markdown: headings, tables, task lists, fenced code with a language, ` + "```mermaid" + ` diagrams, and callouts as ` + "`> **Note**`" + `, ` + "`> **Warning**`" + ` or ` + "`> **Decision**`" + ` blockquotes.
` + documentStyle + `
- Never put these documents in an external document tool or connector (Claude Docs, claude.ai artifacts, Notion, Google Docs, Confluence…) unless the person explicitly asks for that tool by name. Mention the artifact in your reply rather than pasting the whole document again.`

// commandBriefing is the three sections above, as one text.
const commandBriefing = workspaceBriefing + "\n\n" + runBriefing + "\n\n" + documentsBriefing

// cliPlaceholder is replaced by the CLI's full path: the fallback for a shell
// that reaches another lazybox first. A live session started before the shell
// integration (agent_cli.go) gets it with its next message, as an update of
// the shared instructions, and has no LAZYBOX_V2_CLI: the briefing names the
// path itself, never only the variable.
const cliPlaceholder = "{{LAZYBOX_CLI}}"

// briefing is the built-in sections (briefing.go), for this Runner's CLI.
func (b *Bench) briefing() string {
	parts := make([]string, 0, len(builtinBriefings))
	for _, s := range builtinBriefings {
		parts = append(parts, b.cliText(s.text))
	}
	return strings.Join(parts, "\n\n")
}

// cliText writes this Runner's CLI path where text has cliPlaceholder.
func (b *Bench) cliText(text string) string {
	return strings.ReplaceAll(text, cliPlaceholder, filepath.Join(b.Config.CLIDir, "lazybox"))
}

// outputEnv names the execution's directory for full-output files. The CLI
// creates it on first use, and falls back to OutputFallbackRoot when a sandbox
// does not let it write there (Codex's workspace-write allows only the
// workspace and the temp directories).
func (b *Bench) outputEnv(id string) string {
	return "LAZYBOX_V2_OUTPUT_DIR=" + filepath.Join(b.Config.OutputRoot, id)
}

// FinishPipe ends a pipe capture and keeps what a `lazybox run` gave its agent:
// on the command, for the console, and in telemetry, to measure the savings.
func (b *Bench) FinishPipe(c *commands.Command, cause error, signal string, report *commands.Report) error {
	if err := c.FinishPipeSignalled(cause, signal, report); err != nil {
		return err
	}
	if report == nil {
		return nil
	}
	state := c.Status()
	b.recordRun(state, report)
	// A `lazybox run -- gh pr create …` prints the pull request it opened.
	go b.itemPipeFinished(c)
	// A workspace run's view reaches an agent only if a person sends it, so it
	// is not counted with what agents received.
	if b.Telemetry != nil && state.Spec.SessionID != "" {
		run := telemetry.CommandRun{ID: state.ID, Scope: telemetry.Scope{Execution: state.Spec.SessionID}, Filter: report.Filter, ExitCode: report.ExitCode, OutputBytes: report.OutputBytes, AgentBytes: report.AgentBytes}
		if e, err := b.Get(state.Spec.SessionID); err == nil {
			run.Scope.Workspace, run.Scope.Conversation = e.CollectionID, e.Saved.ConversationID
		}
		ctx, cancel := context.WithTimeout(b.ctx, 5*time.Second)
		// The capture is already complete: a lost measurement must not fail it.
		_ = b.Telemetry.PutCommand(ctx, run)
		cancel()
	}
	b.trimOutput()
	return nil
}

// trimOutput bounds full-output files in the background, one sweep at a time.
func (b *Bench) trimOutput() {
	if !b.trimming.CompareAndSwap(false, true) {
		return
	}
	b.trims.Add(1)
	go func() {
		defer b.trims.Done()
		defer b.trimming.Store(false)
		_ = trimOutput([]string{b.Config.OutputRoot, b.Config.OutputFallbackRoot}, b.Config.OutputBudget, time.Now().Add(-outputGrace))
	}()
}

// outputGrace keeps a file the agent was just pointed at, even over budget.
const outputGrace = time.Hour

// trimOutput deletes the oldest full-output files until the roots together
// hold at most budget bytes. Files modified after keep are never deleted, and
// a session directory the sweep empties is removed.
func trimOutput(roots []string, budget int64, keep time.Time) error {
	type file struct {
		path string
		size int64
		mod  time.Time
	}
	var files []file
	var total int64
	var failures []error
	for _, root := range roots {
		if root == "" {
			continue
		}
		err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				if errors.Is(err, fs.ErrNotExist) {
					return nil
				}
				return err
			}
			if !d.Type().IsRegular() {
				return nil
			}
			info, err := d.Info()
			if err != nil {
				return nil // removed while walking
			}
			files = append(files, file{path, info.Size(), info.ModTime()})
			total += info.Size()
			return nil
		})
		if err != nil {
			failures = append(failures, err)
		}
	}
	sort.Slice(files, func(i, j int) bool { return files[i].mod.Before(files[j].mod) })
	for _, f := range files {
		if total <= budget || f.mod.After(keep) {
			break
		}
		if err := os.Remove(f.path); err != nil && !errors.Is(err, fs.ErrNotExist) {
			failures = append(failures, err)
			continue
		}
		total -= f.size
		if dir := filepath.Dir(f.path); !slices.Contains(roots, dir) {
			_ = os.Remove(dir) // succeeds only once the directory is empty
		}
	}
	return errors.Join(failures...)
}

// removeOutput deletes one execution's full-output files, wherever the CLI put them.
func (b *Bench) removeOutput(id string) error {
	var failures []error
	for _, root := range []string{b.Config.OutputRoot, b.Config.OutputFallbackRoot} {
		if root == "" {
			continue
		}
		if err := os.RemoveAll(filepath.Join(root, id)); err != nil {
			failures = append(failures, err)
		}
	}
	return errors.Join(failures...)
}
