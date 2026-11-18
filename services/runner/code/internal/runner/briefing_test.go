package runner

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// Each built-in section's text is pinned to its version. Changing the text
// fails here until the change is a version of its own: add a briefingChange
// (the next version, a phrase it brings, its one-line note) to
// builtinBriefings, then update the pin. That note is how every running
// conversation hears of the change.
func TestEachBuiltinSectionTextIsPinnedToItsVersion(t *testing.T) {
	pins := map[string]struct {
		version int
		sum     string
	}{
		"workspace":     {5, "0da12c2ae8ec8cb0"},
		"run":           {3, "5e6c4cdceaa73f01"},
		"documents":     {3, "68862d591a24041e"},
		"conversations": {2, "9eef1f6b082a54f4"},
	}
	if len(pins) != len(builtinBriefings) {
		t.Fatalf("%d sections, %d pins", len(builtinBriefings), len(pins))
	}
	for _, s := range builtinBriefings {
		sum := sha256.Sum256([]byte(s.text))
		got, pin := hex.EncodeToString(sum[:8]), pins[s.id]
		if got != pin.sum || s.version() != pin.version {
			t.Errorf("%s: text %s at version %d, pinned %s at %d: a changed section needs a new version with its note", s.id, got, s.version(), pin.sum, pin.version)
		}
		for i, c := range s.changes {
			if c.version != i+1 || c.note == "" || !strings.Contains(s.text, c.since) {
				t.Errorf("%s v%d: versions count from 1, each with a note and a phrase still in the text (%q)", s.id, c.version, c.since)
			}
			if len(c.note) > 400 {
				t.Errorf("%s v%d: a note is one short line (%d bytes)", s.id, c.version, len(c.note))
			}
		}
	}
}

// briefed is a Runner with its CLI, so coding agents get the built-in sections.
func briefed(t *testing.T) (*Bench, *agenttest.Adapter, *Execution) {
	t.Helper()
	b, err := New(Config{State: t.TempDir(), CLIDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { b.Close() })
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	return b, fake, e
}

// asIfStartedBefore makes e a conversation from before versions were
// recorded, told context.
func asIfStartedBefore(t *testing.T, b *Bench, e *Execution, context string, briefing map[string]string) {
	t.Helper()
	amend := func(r *sessionstore.Record) { r.Context, r.Briefing, r.Instructions = context, briefing, "" }
	if err := b.History.Amend(t.Context(), e.ID, amend); err != nil {
		t.Fatal(err)
	}
	amend(&e.Saved)
}

// send is a message as the transport sends it: with what is due, then recorded.
func send(t *testing.T, b *Bench, e *Execution, fake *agenttest.Adapter, id string) agent.Operation {
	t.Helper()
	u := b.BriefingForMessage(e)
	if _, err := e.Engine.Apply(context.Background(), agent.Operation{ID: id, Kind: agent.Message, Text: "go on", Context: u.Text}); err != nil {
		t.Fatal(err)
	}
	if err := b.BriefingDelivered(context.Background(), e, u, id); err != nil {
		t.Fatal(err)
	}
	return applied(t, fake.Session)
}

func finishTurn(t *testing.T, e *Execution, fake *agenttest.Adapter) {
	t.Helper()
	fake.Session.Events <- agent.Event{Type: "turn_started", TurnID: "t"}
	fake.Session.Events <- agent.Event{Type: "turn_ended", TurnID: "t", Status: "completed"}
	waitHistory(t, func() bool { return !e.Engine.Snapshot().Busy })
}

func TestANewConversationIsUpToDateAndRecordsItsVersions(t *testing.T) {
	b, _, e := briefed(t)
	if u := b.BriefingUpdate(e); u.Text != "" {
		t.Fatalf("a new conversation already has its briefing: %q", u.Text)
	}
	saved, err := b.History.Get(t.Context(), e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if saved.Briefing["conversations"] != "2" || saved.Briefing["workspace"] != "5" {
		t.Fatalf("versions not recorded at the start: %v", saved.Briefing)
	}
	if !strings.Contains(saved.Context, "--provider codex") || !strings.Contains(saved.Context, "never changes its provider") || !strings.Contains(saved.Context, "only starts copies of yourself") {
		t.Fatalf("the briefing must name providers:\n%s", saved.Context)
	}
	// A conversation from before versions were recorded that was told all of it is up to date too.
	asIfStartedBefore(t, b, e, saved.Context, nil)
	if u := b.BriefingUpdate(e); u.Text != "" {
		t.Fatalf("a conversation told the current briefing hears nothing: %q", u.Text)
	}
}

// The case that started this: a conversation from before `lazybox conversation
// new` hears of it, with providers, once, with its next message, and nothing
// it already knew.
func TestAnOldConversationHearsWhatLazyboxAddedOnceWithItsNextMessage(t *testing.T) {
	b, fake, e := briefed(t)
	conversations := b.cliText(conversationBriefing)
	old := strings.Replace(e.Saved.Context, conversations, "", 1)
	if old == e.Saved.Context {
		t.Fatal("fixture: the conversation section is not in the context")
	}
	asIfStartedBefore(t, b, e, old, nil)

	u := b.BriefingUpdate(e)
	if !slices.Equal(u.Sections, []string{"conversations"}) || !u.Features {
		t.Fatalf("only the changed section: %+v", u)
	}
	for _, want := range []string{briefingPreamble, "with any agent (Claude, Codex, …)", "lazybox conversation new --provider P --title T --brief-artifact ID", "approves in one click", "only starts copies of yourself", "never changes its provider"} {
		if !strings.Contains(u.Text, want) {
			t.Fatalf("the update lacks %q:\n%s", want, u.Text)
		}
	}
	if strings.Count(u.Text, "\n- ") != 1 || strings.Contains(u.Text, "##") || strings.Contains(u.Text, "lazybox run") || len(u.Text) > 800 {
		t.Fatalf("the delta is one short line, never a re-briefing:\n%s", u.Text)
	}

	got := send(t, b, e, fake, "m1")
	if got.Context != u.Text || got.Text != "go on" {
		t.Fatalf("the agent gets the update beside the message: %+v", got)
	}
	journaled(t, e, `"briefing_updated"`, `"operationId":"m1"`, `"status":"features"`, `"conversations":"2"`, `with any agent`)
	saved, err := b.History.Get(t.Context(), e.ID)
	if err != nil || saved.Briefing["conversations"] != "2" || saved.Briefing["workspace"] != "5" {
		t.Fatalf("the versions it now has are durable: %v %v", err, saved.Briefing)
	}

	finishTurn(t, e, fake)
	if again := send(t, b, e, fake, "m2"); again.Context != "" {
		t.Fatalf("an update is sent once: %q", again.Context)
	}
}

func TestAConversationBehindHearsOnlyTheChangesItMissed(t *testing.T) {
	b, _, e := briefed(t)
	current := briefingVersions(b.briefingSections("codex", "", "", false))
	// Told the first conversation line, and the workspace up to version 2.
	behind := map[string]string{}
	for id, v := range current {
		behind[id] = v
	}
	behind["conversations"], behind["workspace"] = "1", "2"
	asIfStartedBefore(t, b, e, e.Saved.Context, behind)
	u := b.BriefingUpdate(e)
	if !slices.Equal(u.Sections, []string{"workspace", "conversations"}) {
		t.Fatalf("sections %v", u.Sections)
	}
	lines := strings.Split(strings.TrimPrefix(u.Text, briefingPreamble+"\n"), "\n")
	if len(lines) != 4 {
		t.Fatalf("workspace versions 3 to 5 and the conversation line, one line each:\n%s", u.Text)
	}
	if want := "`\"" + filepath.Join(b.Config.CLIDir, "lazybox") + "\"`"; !strings.Contains(lines[0], want) {
		t.Fatalf("a note names this Runner's CLI: %q", lines[0])
	}
	if strings.Contains(u.Text, "request-repo") {
		t.Fatalf("version 2 was given already:\n%s", u.Text)
	}

	// Placed from what it was told: the first conversation line, nothing more.
	v1 := "## Starting another conversation\nTo start another conversation (e.g. the person asks you to), run `lazybox conversation new --title \"…\" --brief \"…\"`; the person approves it in one click."
	asIfStartedBefore(t, b, e, strings.Replace(e.Saved.Context, b.cliText(conversationBriefing), v1, 1), nil)
	if u := b.BriefingUpdate(e); !slices.Equal(u.Sections, []string{"conversations"}) || strings.Count(u.Text, "\n- ") != 1 {
		t.Fatalf("a conversation at version 1 hears version 2 only: %+v", u)
	}
}

func TestTheUpdateNeverEntersATurnInProgress(t *testing.T) {
	b, fake, e := briefed(t)
	asIfStartedBefore(t, b, e, strings.Replace(e.Saved.Context, b.cliText(conversationBriefing), "", 1), nil)
	fake.Session.Events <- agent.Event{Type: "turn_started", TurnID: "t"}
	waitHistory(t, func() bool { return e.Engine.Snapshot().Busy })
	if u := b.BriefingForMessage(e); u.Text != "" {
		t.Fatalf("a message sent during a turn waits for it, without the update: %q", u.Text)
	}
	if b.BriefingUpdate(e).Text == "" {
		t.Fatal("the update is still due (the conversation shows it pending)")
	}
	// Queued behind the turn: still nothing.
	if _, err := e.Engine.Apply(context.Background(), agent.Operation{ID: "queued", Kind: agent.Message, Text: "after this"}); err != nil {
		t.Fatal(err)
	}
	if u := b.BriefingForMessage(e); u.Text != "" {
		t.Fatalf("not while a message waits: %q", u.Text)
	}
	fake.Session.Events <- agent.Event{Type: "turn_ended", TurnID: "t", Status: "completed"}
	if got := applied(t, fake.Session); got.ID != "queued" || got.Context != "" {
		t.Fatalf("the queued message goes as it was sent: %+v", got)
	}
	finishTurn(t, e, fake)
	if got := send(t, b, e, fake, "next"); !strings.Contains(got.Context, "only starts copies of yourself") {
		t.Fatalf("the next message that starts a turn carries it: %q", got.Context)
	}
}

func TestChangedSettingsReachRunningSessionsOnceWithTheNextMessage(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if u := b.BriefingUpdate(e); u.Text != "" {
		t.Fatalf("a new session already has the current instructions: %q", u.Text)
	}
	d := b.Preferences.Snapshot()
	d.Context = "Always run the linter."
	if err = b.Preferences.Save(d); err != nil {
		t.Fatal(err)
	}
	u := b.BriefingUpdate(e)
	if !strings.HasPrefix(u.Text, briefingPreamble) || !strings.Contains(u.Text, "Settings → Agents), as they now read\nAlways run the linter.") || u.Features || !slices.Equal(u.Sections, []string{"settings"}) {
		t.Fatalf("stale session not given the new instructions: %+v", u)
	}
	got := send(t, b, e, fake, "message-1")
	if got.Context != u.Text {
		t.Fatalf("provider did not get the update beside the text: %+v", got)
	}
	if b.BriefingUpdate(e).Text != "" {
		t.Fatal("an update is delivered once")
	}
	journaled(t, e, `"briefing_updated"`, `"status":"instructions"`)
	// Retrying the same message must not be refused because the update now differs.
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "message-1", Kind: agent.Message, Text: "go on"}); err != nil {
		t.Fatalf("retry refused: %v", err)
	}
	// Removed: the agent is told to disregard them, once.
	d = b.Preferences.Snapshot()
	d.Context = ""
	if err = b.Preferences.Save(d); err != nil {
		t.Fatal(err)
	}
	finishTurn(t, e, fake)
	if got := send(t, b, e, fake, "message-2"); !strings.Contains(got.Context, "removed their instructions") {
		t.Fatalf("removal not said: %q", got.Context)
	}
	if b.BriefingUpdate(e).Text != "" {
		t.Fatal("a removal is said once")
	}
}

func TestAssistantsAreNotSentBriefingUpdates(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), CLIDir: "/opt/lazybox/bin"})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	e := &Execution{Purpose: "help"}
	e.Saved.ID = "assistant"
	if b.BriefingUpdate(e).Text != "" {
		t.Fatal("assistants are answer-only and get no command briefing")
	}
}
