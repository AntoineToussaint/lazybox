package runner

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"strconv"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// A coding agent's briefing is a list of sections, each with an ID and a
// version (COMMAND_OUTPUT.md, "The briefing"). A conversation is told
// its briefing when it opens, and lives for days, across Runner restarts:
// Lazybox gains capabilities meanwhile. So the Runner records, per
// conversation, the version of each section it was given
// (sessionstore.Record.Briefing), and when one is behind, the conversation's
// next message carries one compact note of what changed: for a built-in
// section, a line per change it missed; for the person's own instructions and
// the conversation's browser, the section as it now reads. Never the whole
// briefing again, never twice for one change, and never into a running turn
// (BriefingForMessage). The person sees the note as one quiet event of the
// conversation (briefing_updated), opened to exactly what the agent received.

// briefingChange is one version of a built-in section.
type briefingChange struct {
	version int
	// since is a phrase this version brought into the section's text. A
	// conversation from before versions were recorded is at the highest
	// version whose phrase what it was told holds.
	since string
	// note says the change in one line, for a conversation behind it.
	note string
	// restates: the note says all that earlier notes did, so a conversation
	// further behind hears only it.
	restates bool
}

// builtinBriefing is a section of the Runner's own text.
type builtinBriefing struct {
	id, text string
	// changes, oldest first: the last is the current version. A change to
	// text adds one (briefing_test.go pins each text to its version).
	changes []briefingChange
}

// builtinBriefings are the Runner's sections, in the order an agent reads them.
var builtinBriefings = []builtinBriefing{
	{id: "workspace", text: workspaceBriefing, changes: []briefingChange{
		{version: 1, since: "## Your Lazybox workspace", note: "You work inside a Lazybox workspace: one git worktree per repository, on this workspace's branch. Do not read or edit repositories elsewhere on disk."},
		{version: 2, since: "lazybox workspace request-repo", note: "`lazybox workspace repos` lists this workspace's repositories. For one it lacks, run `lazybox workspace request-repo <name|owner/repo|path> --reason \"…\"`: the person adds it in one click."},
		{version: 3, since: "means Lazybox's own CLI", note: "If `lazybox --version` does not name Lazybox V2, another lazybox comes first on your PATH: run Lazybox's CLI by its full path, `\"" + cliPlaceholder + "\"`."},
		{version: 4, since: "Lazybox tracks the pull requests you open", note: "Lazybox tracks the pull requests you open and shows the person their checks and reviews. For one opened some other way, run `lazybox link <URL>`; `lazybox items` lists them."},
		{version: 5, since: "it has no TTY", note: "A repository may have a home checkout, the person's own clone (`lazybox repo home <name>`): read-only for you. Run a repository's own tools at latest main there; for logins and other commands that need a terminal, ask the person, written as: run in the home checkout of <repository>: `<command>`."},
		{version: 6, since: "takes over its terminal", note: "If your managed command tool offers the interactive action, use it with the command argv for a workspace login. The person reviews the command and enters credentials in its terminal. The tool returns the exit status."},
	}},
	{id: "run", text: runBriefing, changes: []briefingChange{
		{version: 1, since: "## Running commands in Lazybox", note: "Run tests, builds and other noisy commands as `lazybox run -- <command>`: the person sees the full output; you get a compact view that keeps the failures."},
		{version: 2, since: `"start_action"`, note: "If the workspace defines actions, start its dev server with the command tool's \"start_action\": it gets the workspace's own port, and the person sees it in Run and Preview."},
		{version: 3, since: "never with & or nohup", note: "Start servers and watchers with `lazybox run -- <command>` too, never with & or nohup: one still running after 20s moves to the background (`lazybox ls` lists your commands, `lazybox stop ID` stops one)."},
	}},
	{id: "documents", text: documentsBriefing, changes: []briefingChange{
		{version: 1, since: "lazybox artifact put", note: "Publish plans, specs and reports you write for the person as Lazybox artifacts: `lazybox artifact put <file|-> --title \"…\" --kind plan|spec|report|notes|other`, and each revision as a new version with `--id ID`."},
		{version: 2, since: "## Documents you write for the person", note: "Keep those documents in Lazybox, in GitHub-flavoured Markdown: never in an external document tool or connector unless the person names it."},
		{version: 3, since: "--supersedes", note: "Keep one artifact per topic: merge overlapping ones with `lazybox artifact put <file|-> --title \"…\" --supersedes ID,ID`, or retire one with `lazybox artifact retire ID`; do not leave pointer stubs."},
		{version: 4, since: "Prefer Mermaid for workflows", note: "Prefer Mermaid for workflows, state transitions and service interactions. Show entry points, decisions and relevant failure paths. Use tables for comparisons and checklists for procedures; link to the source and document assumptions and unverified work."},
		{version: 5, since: "lazybox artifact template KIND", note: "For structured artifacts start with lazybox artifact template KIND and publish with --kind KIND --format json. Each supported kind has a versioned schema."},
	}},
	{id: "conversations", text: conversationBriefing, changes: []briefingChange{
		{version: 1, since: "lazybox conversation new", note: "To start another conversation when the person asks, run `lazybox conversation new --title \"…\" --brief \"…\"` (or `--brief-artifact ID`): the person approves it in one click."},
		{version: 2, since: "Your own sub-agent tool only starts copies of yourself", restates: true, note: "Start another conversation with any agent (Claude, Codex, …) when the person asks: `lazybox conversation new --provider P --title T --brief-artifact ID` (or `--brief \"…\"`), which the person approves in one click. Your own sub-agent tool only starts copies of yourself, and switching this conversation's subscription never changes its provider."},
	}},
}

func (s builtinBriefing) version() int { return s.changes[len(s.changes)-1].version }

// briefingSection is one section as a conversation opened now is told it.
type briefingSection struct {
	id, text, version string
	// heading introduces the whole text in an update, for a section without its own.
	heading string
	// changes: a built-in section's history; nil for the person's text and the browser.
	changes []briefingChange
}

// contentVersion versions a section that is not the Runner's text: the
// person's instructions, the conversation's browser.
func contentVersion(text string) string {
	sum := sha256.Sum256([]byte(text))
	return "h" + hex.EncodeToString(sum[:6])
}

// briefingSections is the briefing a coding agent of provider opened now in
// workspace is told, section by section; browser says whether it gets the
// browser tools. The Settings → Agents text and its provider-specific part
// come first, the built-in sections after, the browser last.
func (b *Bench) briefingSections(provider, purpose, workspace string, browser bool) []briefingSection {
	prefs := b.Preferences.Snapshot()
	var out []briefingSection
	dynamic := func(s briefingSection) {
		if s.text = strings.TrimSpace(s.text); s.text != "" {
			s.version = contentVersion(s.text)
			out = append(out, s)
		}
	}
	dynamic(briefingSection{id: "settings", text: prefs.Context, heading: "## Your instructions from the person (Settings → Agents), as they now read"})
	own, name := prefs.ClaudeContext, "Claude"
	if provider == "codex" {
		own, name = prefs.CodexContext, "Codex"
	}
	if accounts.ACPProvider(provider) {
		own = ""
	}
	dynamic(briefingSection{id: "provider", text: own, heading: "## Your instructions for " + name + " (Settings → Agents), as they now read"})
	if b.Config.CLIDir != "" && !agent.IsAssistant(purpose) {
		for _, s := range builtinBriefings {
			out = append(out, briefingSection{id: s.id, text: b.cliText(s.text), version: strconv.Itoa(s.version()), changes: s.changes})
		}
	} else {
		dynamic(briefingSection{id: "document-style", heading: "## How to present workflows and decisions", text: documentStyle})
	}
	dynamic(briefingSection{id: "browser", text: b.browserBriefing(provider, purpose, workspace, browser)})
	return out
}

// goneNotes: what an update says of a section the conversation was given
// that is now empty: only the person's own text is ever removed.
var goneNotes = []struct{ id, note string }{
	{"settings", "The person removed their instructions in Settings → Agents: disregard them."},
	{"provider", "The person removed their provider-specific instructions in Settings → Agents: disregard them."},
}

// sectionText is the text of section id, "" when the briefing has none.
func sectionText(sections []briefingSection, id string) string {
	for _, s := range sections {
		if s.id == id {
			return s.text
		}
	}
	return ""
}

// briefingVersions is what a conversation told sections has, by section.
func briefingVersions(sections []briefingSection) map[string]string {
	versions := make(map[string]string, len(sections))
	for _, s := range sections {
		versions[s.id] = s.version
	}
	return versions
}

// givenVersion is the version of s a conversation has: as recorded, or, for
// one from before versions were recorded, read from what it was told.
func givenVersion(saved sessionstore.Record, s briefingSection) string {
	if saved.Briefing != nil {
		return saved.Briefing[s.id]
	}
	told := saved.Context + "\n" + saved.Instructions
	if strings.Contains(told, s.text) {
		return s.version
	}
	given := ""
	for _, c := range s.changes {
		if strings.Contains(told, c.since) {
			given = strconv.Itoa(c.version)
		}
	}
	return given
}

// notesSince are the notes of the changes after version given, from the last
// one that restates the section.
func notesSince(changes []briefingChange, given string) []string {
	from, _ := strconv.Atoi(given)
	var notes []string
	for _, c := range changes {
		if c.version <= from {
			continue
		}
		if c.restates {
			notes = notes[:0]
		}
		notes = append(notes, c.note)
	}
	return notes
}

// briefingPreamble opens every update: it adds to what the agent was told.
const briefingPreamble = "[Lazybox] Since this conversation started, Lazybox added or changed the following; everything else you were told still holds."

// BriefingUpdate is what a conversation behind on its briefing is sent.
type BriefingUpdate struct {
	// Text is exactly what the agent receives; "" when it is up to date.
	Text string
	// Sections are the IDs of the sections it covers, in briefing order.
	Sections []string
	// Versions is every section's version once it has the update.
	Versions map[string]string
	// Features: a built-in section changed (Lazybox gained something), not
	// only the person's instructions or the browser.
	Features bool
}

// BriefingUpdate returns what e's conversation should hear with its next
// message, or a zero update when it has the current briefing.
func (b *Bench) BriefingUpdate(e *Execution) BriefingUpdate {
	if agent.IsAssistant(e.Purpose) || e.Saved.ID == "" {
		return BriefingUpdate{}
	}
	e.activity.Lock()
	saved := sessionstore.Record{Briefing: e.Saved.Briefing, Context: e.Saved.Context, Instructions: e.Saved.Instructions}
	e.activity.Unlock()
	sections := b.briefingSections(e.Profile.Provider, e.Purpose, e.CollectionID, e.browserTools)
	u := BriefingUpdate{Versions: briefingVersions(sections)}
	var notes, blocks []string
	for _, s := range sections {
		given := givenVersion(saved, s)
		if given == s.version {
			continue
		}
		u.Sections = append(u.Sections, s.id)
		if s.changes == nil {
			blocks = append(blocks, strings.TrimSpace(s.heading+"\n"+s.text))
			continue
		}
		u.Features = true
		for _, note := range notesSince(s.changes, given) {
			notes = append(notes, "- "+b.cliText(note))
		}
	}
	for _, gone := range goneNotes {
		if _, now := u.Versions[gone.id]; !now && saved.Briefing[gone.id] != "" {
			u.Sections = append(u.Sections, gone.id)
			notes = append(notes, "- "+gone.note)
		}
	}
	if len(u.Sections) == 0 {
		return BriefingUpdate{}
	}
	text := briefingPreamble
	if len(notes) > 0 {
		text += "\n" + strings.Join(notes, "\n")
	}
	if len(blocks) > 0 {
		text += "\n\n" + strings.Join(blocks, "\n\n")
	}
	u.Text = text
	return u
}

// BriefingForMessage is the update to send with a message to e now: none
// while a turn runs or messages wait for one. The note never enters a turn
// in progress; it goes with the next message that starts one.
func (b *Bench) BriefingForMessage(e *Execution) BriefingUpdate {
	if s := e.Engine.Snapshot(); s.Busy || s.Queued > 0 {
		return BriefingUpdate{}
	}
	return b.BriefingUpdate(e)
}

// BriefingDelivered records that e's conversation received u with the
// message operationID: durably, so it is never sent again, and in its
// journal, where the person sees it as one quiet event.
func (b *Bench) BriefingDelivered(ctx context.Context, e *Execution, u BriefingUpdate, operationID string) error {
	if u.Text == "" {
		return nil
	}
	if err := b.History.Amend(ctx, e.ID, func(r *sessionstore.Record) { r.Briefing = u.Versions }); err != nil {
		return err
	}
	e.activity.Lock()
	e.Saved.Briefing = u.Versions
	e.activity.Unlock()
	status := "instructions"
	if u.Features {
		status = "features"
	}
	data, _ := json.Marshal(map[string]any{"sections": u.Sections, "versions": u.Versions})
	return b.Record(e, agent.Event{Type: "briefing_updated", Text: u.Text, Status: status, OperationID: operationID, Data: data})
}
