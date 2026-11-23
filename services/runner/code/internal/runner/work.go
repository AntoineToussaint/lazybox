package runner

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/snippets"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Missions live in <State>/work: one log per mission, and the Runner-wide
// prompt log. They are local to this Runner (ORCHESTRATION.md decision 8: the
// event envelope is what Lazybox offers the external Hub, not a second Hub).
func (b *Bench) openWork() error {
	dir := filepath.Join(b.Config.State, "work")
	store, err := work.Open(dir)
	if err != nil {
		return err
	}
	prompts, err := work.OpenPrompts(filepath.Join(dir, "prompts.jsonl"))
	if err != nil {
		_ = store.Close()
		return err
	}
	b.Work, b.Prompts = store, prompts
	if err := b.openDelivery(); err != nil {
		_ = store.Close()
		_ = prompts.Close()
		b.Work, b.Prompts = nil, nil
		return err
	}
	return nil
}

func (b *Bench) closeWork() error {
	var errs []error
	if b.Work != nil {
		errs = append(errs, b.Work.Close())
	}
	if b.Prompts != nil {
		errs = append(errs, b.Prompts.Close())
	}
	if b.Delivery != nil {
		errs = append(errs, b.Delivery.Close())
	}
	return errors.Join(errs...)
}

// Observe is the liveness of the conversations that are live attempts: what
// a board is derived with besides the log. A conversation with no live
// session is closed; its latest session is found in history.
func (b *Bench) Observe(ctx context.Context, conversations []string) work.Observations {
	obs := work.Observations{Conversations: map[string]work.Liveness{}}
	want := map[string]bool{}
	for _, c := range conversations {
		want[c] = true
	}
	// List is newest first: the first execution seen is the conversation's latest.
	for _, e := range b.List() {
		c := conversationOf(e)
		if !want[c] {
			continue
		}
		if _, seen := obs.Conversations[c]; seen {
			continue
		}
		l := liveness(e)
		// A rename is in history, not in the execution's saved record.
		if record, err := b.History.Get(ctx, e.ID); err == nil && record.DisplayTitle != "" {
			l.Title = record.DisplayTitle
		}
		obs.Conversations[c] = l
	}
	for c := range want {
		if _, ok := obs.Conversations[c]; ok {
			continue
		}
		latest, title, err := b.latestSession(ctx, c)
		switch {
		case errors.Is(err, os.ErrNotExist):
			obs.Conversations[c] = work.Liveness{State: work.LiveMissing, Detail: "the conversation was deleted"}
		default:
			obs.Conversations[c] = work.Liveness{State: work.LiveClosed, Session: latest, Title: title}
		}
	}
	return obs
}

func (b *Bench) latestSession(ctx context.Context, conversation string) (string, string, error) {
	if b.History == nil {
		return "", "", os.ErrNotExist
	}
	if ids, err := b.History.Deleted(ctx, conversation); err == nil && len(ids) > 0 {
		return "", "", os.ErrNotExist
	}
	record, err := b.History.Get(ctx, conversation)
	if err != nil {
		return "", "", err
	}
	id, title := record.ID, firstOf(record.DisplayTitle, record.Title)
	for range 64 {
		next, err := b.History.Successor(ctx, id)
		if err != nil {
			break
		}
		id = next.ID
	}
	return id, title, nil
}

func liveness(e *Execution) work.Liveness {
	s := e.Engine.Snapshot()
	l := work.Liveness{Session: e.ID, Title: firstOf(e.Saved.DisplayTitle, s.Title, e.Saved.Title)}
	switch {
	case s.Closed || s.Status == agent.Stopped || (s.Status == agent.Failed && !agent.LimitFault(s.Fault) && !agent.TurnFault(s.Fault)):
		l.State, l.Detail = work.LiveClosed, s.Fault
	// A subscription limit waits for a switch or its reset; a provider error
	// that ended a turn waits for the next message. Neither ended the
	// conversation.
	case s.Status == agent.Failed && agent.LimitFault(s.Fault):
		l.State, l.Detail = work.LiveWaiting, s.Fault
	case s.Status == agent.Failed:
		l.State, l.Detail = work.LiveIdle, s.Fault
	case len(s.Pending) > 0 || s.Status == agent.WaitingInput || s.Status == agent.WaitingApproval:
		l.State = work.LiveWaiting
		for _, p := range s.Pending {
			if p.Type == "approval" {
				l.Detail = "approve " + strings.TrimSpace(p.Text)
			} else if l.Detail == "" {
				l.Detail = "a question"
			}
		}
	case s.Busy || s.Status == agent.Running || s.Status == agent.Starting || s.Status == agent.Switching:
		l.State = work.LiveRunning
		if root, ok := s.Agents["root"]; ok {
			l.Detail = firstOf(root.Tool, root.Phase)
		}
	default:
		l.State = work.LiveIdle
	}
	return l
}

func firstOf(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

// recordRun attaches a finished `lazybox run` (or `lazybox log`) to the live
// attempts it belongs to: an agent's own run to its conversation's attempts,
// a workspace terminal's run to the attempts in that workspace. Evidence is
// recorded by Lazybox, which observed the exit status; it is never a claim.
func (b *Bench) recordRun(state commands.Status, report *commands.Report) {
	if b.Work == nil || report == nil {
		return
	}
	ev := work.Evidence{Kind: work.EvidenceRun, Title: state.Spec.Title, Command: state.ID, OutputPath: report.OutputPath, Piped: report.Piped, Outcome: work.OutcomeUnknown}
	if !report.Piped {
		code := report.ExitCode
		ev.ExitCode = &code
		ev.Outcome = work.OutcomePass
		if code != 0 {
			ev.Outcome = work.OutcomeFail
		}
	}
	if report.OutputBytes > 0 {
		ev.Detail = fmt.Sprintf("agent saw %s of %s", bytesLabel(report.AgentBytes), bytesLabel(report.OutputBytes))
		if report.Filter != "" {
			ev.Detail += " (" + report.Filter + " filter)"
		}
	}
	actor := work.Actor{Kind: work.ActorLazybox}
	var origin work.Origin
	switch {
	case state.Spec.SessionID != "":
		e, err := b.Get(state.Spec.SessionID)
		if err != nil {
			return
		}
		origin.Conversation = conversationOf(e)
		ev.Execution, ev.Workspace = e.ID, e.CollectionID
		actor.Conversation, actor.Execution = origin.Conversation, e.ID
		ev.Ref = "run:" + e.ID + "/" + state.ID
	case state.Spec.CollectionID != "":
		origin.Workspace, ev.Workspace = state.Spec.CollectionID, state.Spec.CollectionID
		ev.Ref = "run:" + state.Spec.CollectionID + "/" + state.ID
	default:
		return
	}
	if !b.hasAttempt(origin) {
		return
	}
	ev.Revisions = b.revisions(ev.Workspace)
	// The capture is complete either way: a lost record must not fail it.
	_, _ = b.Work.Record(origin, actor, ev)
}

func (b *Bench) hasAttempt(origin work.Origin) bool {
	for _, at := range b.Work.Attempts() {
		if (origin.Conversation != "" && at.Attempt.Conversation == origin.Conversation) || (origin.Conversation == "" && origin.Workspace != "" && at.Attempt.Workspace == origin.Workspace) {
			return true
		}
	}
	return false
}

// revisions pins a run to each workspace repository's HEAD, so evidence from
// before a later commit can be told apart.
func (b *Bench) revisions(workspace string) map[string]string {
	if workspace == "" || b.Collections == nil {
		return nil
	}
	c, err := b.Collections.Get(workspace)
	if err != nil {
		return nil
	}
	ctx, cancel := context.WithTimeout(b.ctx, 3*time.Second)
	defer cancel()
	out := map[string]string{}
	for _, m := range c.Members {
		if !m.Ready {
			continue
		}
		if head, err := (gitpublish.Git{}).Head(ctx, m.Path); err == nil {
			out[m.Name] = head
		}
	}
	if len(out) == 0 {
		return nil
	}
	return out
}

// RecordCommit attaches a commit made through the PR loop to the attempts in
// its workspace.
func (b *Bench) RecordCommit(workspace, member, commit, subject string) {
	if b.Work == nil || commit == "" {
		return
	}
	_, _ = b.Work.Record(work.Origin{Workspace: workspace}, work.Actor{Kind: work.ActorLazybox}, work.Evidence{Kind: work.EvidenceCommit, Ref: "commit:" + member + "@" + commit, Title: subject, Outcome: work.OutcomeUnknown, Workspace: workspace, Member: member, Head: commit})
}

// RefreshMissionPullRequest reads the explicitly delivered PR by identity, even
// if the workspace has switched branches since the task linked it.
func (b *Bench) RefreshMissionPullRequest(ctx context.Context, link work.Evidence) {
	ref := workitems.GitHubRef(link.Repository, link.Number)
	if _, err := b.DeliveredLink(link.Workspace, ref); err != nil {
		return
	}
	// Accepted provider reads publish evidence via observeItems. A reaction may
	// still use refreshNow's cached return value, but it cannot verify a task.
	b.refreshNow(ctx, ref)
}

func (b *Bench) recordPullRequest(link workitems.Link, item workitems.Item) error {
	pr := item.PR
	if pr == nil || item.Number == 0 || pr.HeadSHA == "" {
		return nil
	}
	ev := work.Evidence{Kind: work.EvidencePR, Ref: fmt.Sprintf("pr:%s#%d@%s", item.Repository, item.Number, pr.HeadSHA), Title: item.Title, Workspace: link.Workspace, Member: link.Member, Repository: item.Repository, Number: item.Number, Head: pr.HeadSHA, State: item.RawState, URL: item.URL}
	switch pr.Checks {
	case "success":
		ev.Outcome, ev.Detail = work.OutcomePass, fmt.Sprintf("%d checks passed", len(pr.CheckRuns))
	case "failure":
		ev.Outcome, ev.Detail = work.OutcomeFail, "failed: "+strings.Join(pr.FailingNames(), ", ")
	case "pending":
		ev.Outcome, ev.Detail = work.OutcomePending, "checks running"
	default:
		ev.Outcome, ev.Detail = work.OutcomeUnknown, "no checks"
	}
	_, err := b.Work.Record(work.Origin{Workspace: link.Workspace}, work.Actor{Kind: work.ActorCheck}, ev)
	return err
}

// RememberSnippets keeps the catalog the person last listed, to record which
// snippet a message was and the chain it offers next.
func (b *Bench) RememberSnippets(entries []snippets.Entry) {
	copied := append([]snippets.Entry(nil), entries...)
	b.snippetCatalog.Store(&copied)
}

func (b *Bench) snippet(key, text string) (snippets.Entry, bool) {
	catalog := b.snippetCatalog.Load()
	if catalog == nil {
		return snippets.Entry{}, false
	}
	trimmed := strings.TrimSpace(text)
	for _, entry := range *catalog {
		if (key != "" && entry.Key == key) || (key == "" && trimmed != "" && strings.TrimSpace(entry.Body) == trimmed) {
			return entry, true
		}
	}
	return snippets.Entry{}, false
}

func digest(text string) string {
	sum := sha256.Sum256([]byte(text))
	return hex.EncodeToString(sum[:6])
}

// RecordPrompt keeps a message delivered to a conversation in the prompt log,
// with its source. A message the client did not label, whose text is exactly
// a catalog snippet's body, is recorded as that snippet (Matched).
func (b *Bench) RecordPrompt(e *Execution, operation, text string, source work.PromptSource) {
	if b.Prompts == nil {
		return
	}
	var next []string
	if source.Kind == "" || source.Kind == work.SourceTyped || source.Kind == work.SourceSnippet {
		if entry, ok := b.snippet(source.Snippet, text); ok && (source.Kind == work.SourceSnippet || strings.TrimSpace(entry.Body) == strings.TrimSpace(text)) {
			source.Matched = source.Kind != work.SourceSnippet
			source.Kind, source.Snippet, source.Digest, next = work.SourceSnippet, entry.Key, digest(entry.Body), entry.Next
		}
	}
	_ = b.Prompts.Submit(work.Human, work.PromptPayload{Conversation: conversationOf(e), Workspace: e.CollectionID, Execution: e.ID, Text: text, Source: source, Operation: operation}, next)
}

// forgetConversation ends a deleted conversation's attempts and removes its
// prompts: deleting a conversation deletes what was said in it.
func (b *Bench) forgetConversation(ids ...string) {
	if b.Work != nil {
		for _, id := range ids {
			_ = b.Work.EndAttempts(id, "deleted")
			// A deleted conversation runs no more agents.
			_ = b.Work.RevokeHeld(id, "the conversation was deleted")
		}
	}
	if b.Prompts != nil {
		_ = b.Prompts.Forget(ids...)
	}
}

func bytesLabel(n uint64) string {
	switch {
	case n >= 1<<20:
		return fmt.Sprintf("%.1f MiB", float64(n)/(1<<20))
	case n >= 1<<10:
		return fmt.Sprintf("%.1f KiB", float64(n)/(1<<10))
	}
	return fmt.Sprintf("%d B", n)
}

// AttemptConversations lists the conversations of every live attempt, sorted.
func (b *Bench) AttemptConversations() []string {
	if b.Work == nil {
		return nil
	}
	seen := map[string]bool{}
	var out []string
	for _, at := range b.Work.Attempts() {
		if !seen[at.Attempt.Conversation] {
			seen[at.Attempt.Conversation] = true
			out = append(out, at.Attempt.Conversation)
		}
	}
	sort.Strings(out)
	return out
}
