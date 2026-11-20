package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/preferences"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// One-click cross-agent review (GITHUB.md, "Agent review"). A person asks
// for a review of a conversation's work (its changes, one turn, its pull
// request); the Runner opens a read-only conversation in the same workspace
// on another agent (the other provider by default), names it, links it to the
// source, and sends it a brief built from what the Runner already records.
// The reviewer files findings with `lazybox review add`: they land in the
// workspace's review as comments written by that agent, anchored to the lines
// they quote. Nothing is sent to the author until the person sends it.

// ErrNotReviewer refuses `lazybox review add` from a conversation that is not
// reviewing anything.
var ErrNotReviewer = errors.New("this conversation is not reviewing anything: findings come from a review Lazybox started (Changes → Review with…)")

// PromptSourceReview labels what Lazybox sends for a review.
const PromptSourceReview = "review"

// ReviewRequest is what the person asked to review, and by whom.
type ReviewRequest struct {
	TemplateID string
	// Workspace may be empty: the source conversation's.
	Workspace string
	// Source is the conversation (an execution of it) whose work is reviewed.
	Source string
	// Scope is reviews.ScopeChanges, ScopeTurn or ScopePR.
	Scope   string
	Compare string
	Turn    int
	Member  string
	// Profile, else Provider, else the default reviewer setting.
	Profile, Provider string
	// Model the reviewer runs on; "" is the setting's (fixed), else its agent's default.
	Model string
	// Focus is what the person asked the reviewer to look at in particular.
	Focus string
	// PullRequest is what the PR loop knows of the member's pull request.
	PullRequest *ReviewPullRequest
	// Auto: started because a turn ended with changes, not by a click.
	Auto bool
}

// ReviewPullRequest is a pull request under review.
type ReviewPullRequest struct {
	Number      int64
	URL, Title  string
	Base, Head  string
	Description string
}

// ProviderLabel is how people name a provider's agent.
func ProviderLabel(provider string) string {
	switch provider {
	case "codex":
		return "Codex"
	case "claude":
		return "Claude"
	}
	if provider = strings.TrimPrefix(provider, "acp:"); provider == "" {
		return "the agent"
	}
	return strings.ToUpper(provider[:1]) + provider[1:]
}

// otherProvider is the agent that reviews provider's work by default:
// Codex reviews Claude's, Claude reviews Codex's and an ACP agent's.
func otherProvider(provider string) string {
	if provider == "claude" {
		return "codex"
	}
	return "claude"
}

// agentLabel names the agent a profile runs: its provider's name, or an ACP
// agent's own (its profile's label).
func agentLabel(p accounts.Profile) string {
	if accounts.ACPProvider(p.Provider) && p.Label != "" {
		return p.Label
	}
	return ProviderLabel(p.Provider)
}

// reviewer is who reviews: the profile, the model ("" its default) and, when
// it is not the one asked for, why (one sentence the review shows).
type reviewer struct {
	profile  accounts.Profile
	model    string
	fallback string
}

// reviewerReady reports whether a provider's agent can review now: it has a
// profile, its program is not known to be missing (the last probe; none is
// run here) and not every one of its subscriptions is known to need a new
// sign-in. The best such profile comes back: a subscription Lazybox manages
// first (it can switch at a limit).
func (b *Bench) reviewerReady(provider string) (accounts.Profile, bool) {
	if b.Availability != nil {
		if program, probed := b.Availability.Cached()[provider]; probed && (!program.Installed || program.Error != "") {
			return accounts.Profile{}, false
		}
	}
	var chosen *accounts.Profile
	profiles := b.Profiles()
	for i, p := range profiles {
		if p.Provider != provider || (b.Accounts != nil && b.Accounts.Observation(p.ID).State == "auth_required") {
			continue
		}
		if chosen == nil || (p.Managed && !chosen.Managed) {
			chosen = &profiles[i]
		}
	}
	if chosen == nil {
		return accounts.Profile{}, false
	}
	return *chosen, true
}

// knownProvider: Claude, Codex or an ACP agent ("acp:<id>").
func knownProvider(provider string) bool {
	return provider == "claude" || provider == "codex" || accounts.ACPProvider(provider)
}

// reviewerProfile picks who reviews: the profile asked for, else one of the
// provider asked for, else the default reviewer setting (Settings → Agents &
// workspaces → Review by another agent → Default reviewer): the other agent
// than the author's (the default), the author's own agent, or a fixed one.
// A default reviewer that is not available gives way to the author's own
// agent, in a fresh, independent conversation, and the review says so.
func (b *Bench) reviewerProfile(source *Execution, req ReviewRequest) (reviewer, error) {
	profiles := b.Profiles()
	byID := func(id string) (accounts.Profile, bool) {
		for _, p := range profiles {
			if p.ID == id {
				return p, true
			}
		}
		return accounts.Profile{}, false
	}
	if req.Profile != "" {
		p, ok := byID(req.Profile)
		if !ok {
			return reviewer{}, fmt.Errorf("subscription %q is not signed in on this Runner", req.Profile)
		}
		return reviewer{profile: p, model: req.Model}, nil
	}
	if req.Provider != "" {
		if !knownProvider(req.Provider) {
			return reviewer{}, errors.New(`the reviewer is "claude", "codex" or an ACP agent ("acp:<id>")`)
		}
		p, ok := b.reviewerReady(req.Provider)
		if !ok {
			return reviewer{}, fmt.Errorf("%s is not available on this Runner (not installed, or not signed in); add it in Settings → Subscriptions or Agents", ProviderLabel(req.Provider))
		}
		return reviewer{profile: p, model: req.Model}, nil
	}
	var prefs preferences.Review
	if b.Preferences != nil {
		prefs = b.Preferences.Snapshot().Review
	}
	author := source.Profile.Provider
	// The agent the setting asks for; a fixed reviewer that is not available
	// is named in the fallback sentence, and the other agent stands in.
	wanted, missing := otherProvider(author), ""
	switch prefs.ReviewerModeOf() {
	case preferences.ReviewerSame:
		wanted = author
	case preferences.ReviewerFixed:
		model := firstOf(req.Model, prefs.ReviewerModel)
		if p, ok := byID(prefs.Reviewer); ok {
			if _, ready := b.reviewerReady(p.Provider); ready && (b.Accounts == nil || b.Accounts.Observation(p.ID).State != "auth_required") {
				return reviewer{profile: p, model: model}, nil
			}
			missing = agentLabel(p)
			if p.Label != "" && !accounts.ACPProvider(p.Provider) {
				missing += " · " + p.Label
			}
		} else if knownProvider(prefs.Reviewer) {
			if p, ok := b.reviewerReady(prefs.Reviewer); ok {
				return reviewer{profile: p, model: model}, nil
			}
			missing = ProviderLabel(prefs.Reviewer)
		} else {
			missing = "The reviewer you chose"
		}
	}
	fallback := func(p accounts.Profile) reviewer {
		who := firstOf(missing, ProviderLabel(wanted))
		if p.Provider == author {
			return reviewer{profile: p, model: req.Model, fallback: fmt.Sprintf("%s is not available, so a fresh %s conversation reviews.", who, agentLabel(p))}
		}
		return reviewer{profile: p, model: req.Model, fallback: fmt.Sprintf("%s is not available, so %s reviews.", who, agentLabel(p))}
	}
	if p, ok := b.reviewerReady(wanted); ok {
		if missing != "" {
			return fallback(p), nil
		}
		return reviewer{profile: p, model: req.Model}, nil
	}
	for _, provider := range []string{author, "claude", "codex"} {
		if p, ok := b.reviewerReady(provider); ok {
			return fallback(p), nil
		}
	}
	return reviewer{}, errors.New("no agent is available on this Runner to review with")
}

// reviewScope is what the brief says about the scope.
type reviewScope struct {
	label, diff string
	input       []reviews.ReviewInput
	files       []string
}

func plural(n int, one, many string) string {
	if n == 1 {
		return "1 " + one
	}
	return fmt.Sprintf("%d %s", n, many)
}

// scopeOf describes what is reviewed: its label (in the title), how to read
// its diff, and its files, per workspace repository.
func (b *Bench) scopeOf(ctx context.Context, source *Execution, workspace collections.Collection, req ReviewRequest) (reviewScope, error) {
	git := gitinspect.Git{}
	var out reviewScope
	members := []collections.Member{}
	for _, m := range workspace.Members {
		if m.Ready {
			members = append(members, m)
		}
	}
	if len(members) == 0 {
		return out, errors.New("this workspace has no repository ready to review")
	}
	var diff []string
	switch req.Scope {
	case reviews.ScopeTurn:
		if b.Checkpoints == nil {
			return out, errors.New("turn checkpoints are not enabled on this Runner, so one turn cannot be reviewed; review the changes instead")
		}
		record, err := b.Checkpoints.List(conversationOf(source))
		if err != nil {
			return out, err
		}
		var turn *struct{ before, after map[string]string }
		for _, t := range record.Turns {
			if t.Seq != req.Turn {
				continue
			}
			if t.After == nil {
				return out, fmt.Errorf("turn %d has not ended yet; review it once it has", req.Turn)
			}
			turn = &struct{ before, after map[string]string }{map[string]string{}, map[string]string{}}
			for _, r := range t.Before.Repos {
				turn.before[r.Member] = r.Commit
			}
			for _, r := range t.After.Repos {
				turn.after[r.Member] = r.Commit
			}
		}
		if turn == nil {
			return out, fmt.Errorf("this conversation has no turn %d", req.Turn)
		}
		out.label = fmt.Sprintf("Turn %d", req.Turn)
		changes, _, err := b.Checkpoints.Changes(ctx, conversationOf(source), req.Turn)
		if err != nil {
			return out, err
		}
		for _, repo := range changes {
			for _, f := range repo.Files {
				out.files = append(out.files, fmt.Sprintf("%s/%s (%s, +%d −%d)", repo.Member, f.Path, f.Status, f.Additions, f.Deletions))
			}
		}
		for _, m := range members {
			before, after := turn.before[m.Name], turn.after[m.Name]
			if before != "" && after != "" && before != after {
				diff = append(diff, fmt.Sprintf("    git -C %s diff %s %s", shellQuote(m.Path), before, after))
			}
		}
		if len(diff) == 0 {
			diff = append(diff, "    (The turn's checkpoints show no change.)")
		}
		out.diff = "This is one agent turn: the workspace's files before it and after it are saved as checkpoint commits. Read exactly what the turn changed with:\n\n" + strings.Join(diff, "\n") + "\n\nThe files may have changed since; review the turn, and read the files as they are now for context."
		return out, nil
	case reviews.ScopePR:
		pr := req.PullRequest
		if pr == nil || pr.Number == 0 {
			return out, errors.New("this branch has no pull request to review")
		}
		member, ok := findMember(members, req.Member)
		if !ok {
			return out, fmt.Errorf("repository %q is not in this workspace", req.Member)
		}
		out.label = fmt.Sprintf("PR #%d", pr.Number)
		base := "origin/" + strings.TrimPrefix(pr.Base, "origin/")
		committed, err := git.Committed(ctx, member.Path, base)
		if err == nil {
			for _, f := range committed.Files {
				out.files = append(out.files, fmt.Sprintf("%s/%s (%s)", member.Name, f.Path, f.IndexStatus))
			}
		}
		text := fmt.Sprintf("Pull request #%d", pr.Number)
		if pr.Title != "" {
			text += " “" + pr.Title + "”"
		}
		if pr.URL != "" {
			text += " (" + pr.URL + ")"
		}
		text += fmt.Sprintf(", %s into %s, in %s. Read its diff with:\n\n    git -C %s diff %s...HEAD", firstOf(pr.Head, "this branch"), pr.Base, member.Name, shellQuote(member.Path), base)
		if pr.Description != "" {
			text += "\n\nIts description:\n\n" + quoteBlock(clipText(pr.Description, 4<<10))
		}
		out.diff = text
		return out, nil
	}
	// The workspace's changes: since it started (committed and uncommitted),
	// against a branch (from where they diverged), or uncommitted only.
	switch req.Compare {
	case "":
		out.label = "Changes"
	case ":uncommitted":
		out.label = "Uncommitted changes"
	default:
		out.label = "Changes vs " + req.Compare
	}
	for _, m := range members {
		status, err := git.Status(ctx, m.Path)
		if err != nil {
			continue
		}
		seen := map[string]bool{}
		if req.Compare != ":uncommitted" {
			base := firstOf(req.Compare, m.BaseCommit)
			if committed, err := git.Committed(ctx, m.Path, base); err == nil {
				for _, f := range committed.Files {
					seen[f.Path] = true
					out.files = append(out.files, fmt.Sprintf("%s/%s (%s)", m.Name, f.Path, strings.TrimSpace(f.IndexStatus)))
				}
				if len(committed.Files) > 0 {
					from := base
					if req.Compare != "" {
						from = "$(git -C " + shellQuote(m.Path) + " merge-base " + req.Compare + " HEAD)"
					}
					diff = append(diff, fmt.Sprintf("    git -C %s diff %s", shellQuote(m.Path), from))
				}
			}
		}
		uncommitted := 0
		for _, f := range status.Files {
			if seen[f.Path] {
				continue
			}
			uncommitted++
			state := strings.TrimSpace(f.IndexStatus + f.WorktreeStatus)
			if state == "??" {
				state = "untracked"
			}
			out.files = append(out.files, fmt.Sprintf("%s/%s (%s)", m.Name, f.Path, state))
		}
		if req.Compare == ":uncommitted" && len(status.Files) > 0 {
			diff = append(diff, fmt.Sprintf("    git -C %s diff HEAD", shellQuote(m.Path)))
		} else if uncommitted > 0 && !slices.ContainsFunc(diff, func(line string) bool { return strings.Contains(line, shellQuote(m.Path)) }) {
			diff = append(diff, fmt.Sprintf("    git -C %s diff HEAD", shellQuote(m.Path)))
		}
	}
	if len(diff) == 0 {
		diff = append(diff, "    (No change found; tell the person in your summary.)")
	}
	out.diff = "Read the changes (committed and uncommitted, untracked files are listed below) with:\n\n" + strings.Join(diff, "\n")
	return out, nil
}

func findMember(members []collections.Member, name string) (collections.Member, bool) {
	for _, m := range members {
		if m.Name == name {
			return m, true
		}
	}
	if name == "" && len(members) == 1 {
		return members[0], true
	}
	return collections.Member{}, false
}

func shellQuote(path string) string {
	if path != "" && !strings.ContainsAny(path, " '\"$`\\!*?[]{}()&;|<>~#") {
		return path
	}
	return "'" + strings.ReplaceAll(path, "'", `'\''`) + "'"
}

func clipText(text string, limit int) string {
	text = strings.TrimSpace(text)
	if len(text) > limit {
		return strings.TrimSpace(text[:limit]) + " …"
	}
	return text
}

func quoteBlock(text string) string {
	lines := strings.Split(text, "\n")
	for i, l := range lines {
		lines[i] = strings.TrimRight("> "+l, " ")
	}
	return strings.Join(lines, "\n")
}

// reviewGoal is the source conversation's first prompt: from the prompt log,
// else from its first checkpointed turn.
func (b *Bench) reviewGoal(source *Execution) string {
	conversation := conversationOf(source)
	if b.Prompts != nil {
		if history := b.Prompts.History(work.HistoryQuery{Conversation: conversation, Limit: 500}); len(history) > 0 {
			return history[len(history)-1].Text
		}
	}
	if b.Checkpoints != nil {
		if record, err := b.Checkpoints.List(conversation); err == nil {
			for _, t := range record.Turns {
				if t.Prompt != "" {
					return t.Prompt
				}
			}
		}
	}
	return ""
}

// reviewPlan is the workspace's latest plan artifact that is not retired, titled.
func (b *Bench) reviewPlan(workspace string) string {
	if b.Artifacts == nil {
		return ""
	}
	list, err := b.Artifacts.List(workspace)
	if err != nil {
		return ""
	}
	var latest string
	var at int64
	for _, a := range list {
		if a.Kind == "plan" && a.Retired == nil && a.UpdatedAt >= at {
			latest, at = a.ID, a.UpdatedAt
		}
	}
	if latest == "" {
		return ""
	}
	doc, err := b.Artifacts.Get(workspace, latest, 0)
	if err != nil {
		return ""
	}
	return fmt.Sprintf("“%s” (artifact %s, version %d; `lazybox artifact get %s` reads it whole):\n\n%s", doc.Title, doc.ID, doc.ContentVersion, doc.ID, doc.Content)
}

// reviewEvidence is the source conversation's `lazybox run` reports.
func (b *Bench) reviewEvidence(source *Execution) []string {
	var out []string
	ids := map[string]bool{source.ID: true}
	if source.Previous != "" {
		ids[source.Previous] = true
	}
	for id := range ids {
		for _, s := range b.Commands.List(id) {
			r := s.Report
			if r == nil {
				continue
			}
			title := firstOf(s.Spec.Title, strings.Join(s.Spec.Argv, " "), s.ID)
			outcome := "piped output, no exit status"
			if !r.Piped {
				outcome = fmt.Sprintf("exit %d", r.ExitCode)
				if r.ExitCode == 0 {
					outcome = "passed (exit 0)"
				} else {
					outcome = fmt.Sprintf("failed (exit %d)", r.ExitCode)
				}
			}
			line := fmt.Sprintf("`%s` · %s", strings.ReplaceAll(title, "`", "'"), outcome)
			if r.OutputPath != "" {
				line += " · full output: " + r.OutputPath
			}
			out = append(out, line)
		}
	}
	return out
}

// StartReview opens the reviewer conversation and sends it the brief. The
// review is recorded before the brief is sent, so the reviewer's first
// finding finds it; a brief that cannot be delivered fails the review.
func (b *Bench) StartReview(ctx context.Context, req ReviewRequest) (*Execution, reviews.Run, error) {
	if b.Reviews == nil || b.Collections == nil {
		return nil, reviews.Run{}, errors.New("reviews are not available on this Runner")
	}
	source, err := b.Get(req.Source)
	if err != nil {
		return nil, reviews.Run{}, err
	}
	if source, err = b.latest(source); err != nil {
		return nil, reviews.Run{}, err
	}
	if agent.IsAssistant(source.Purpose) {
		return nil, reviews.Run{}, errors.New("an assistant conversation has no work to review")
	}
	if source.CollectionID == "" {
		return nil, reviews.Run{}, errors.New("only a conversation in a Lazybox workspace can be reviewed")
	}
	if req.Workspace != "" && req.Workspace != source.CollectionID {
		return nil, reviews.Run{}, errors.New("that conversation works in another workspace")
	}
	if _, isReviewer := b.Reviews.ReviewerRun(source.ID, conversationOf(source)); isReviewer {
		return nil, reviews.Run{}, errors.New("this conversation is itself a review; review the conversation it reviews instead")
	}
	workspace, err := b.Collections.Get(source.CollectionID)
	if err != nil {
		return nil, reviews.Run{}, err
	}
	if req.Scope == "" {
		req.Scope = reviews.ScopeChanges
	}
	if req.Scope != reviews.ScopeChanges && req.Scope != reviews.ScopeTurn && req.Scope != reviews.ScopePR {
		return nil, reviews.Run{}, errors.New(`the scope is "changes", "turn" or "pr"`)
	}
	preset, err := reviews.TemplateFor(req.TemplateID)
	if err != nil {
		return nil, reviews.Run{}, err
	}
	chosen, err := b.reviewerProfile(source, req)
	if err != nil {
		return nil, reviews.Run{}, err
	}
	profile, fallback := chosen.profile, chosen.fallback
	if req.TemplateID != "" && profile.Provider != "claude" && profile.Provider != "codex" {
		return nil, reviews.Run{}, errors.New("review templates require a Claude or Codex reviewer with read-only controls")
	}
	focus := strings.TrimSpace(req.Focus)
	if len(focus) > reviews.FocusLimit {
		return nil, reviews.Run{}, fmt.Errorf("a review's focus is limited to %d bytes", reviews.FocusLimit)
	}
	if req.TemplateID != "" && (req.Scope != reviews.ScopeChanges || b.Checkpoints == nil) {
		return nil, reviews.Run{}, errors.New("review templates need workspace changes and code checkpoints")
	}
	var required []work.BaselineMember
	var missionID, taskID, attemptID string
	var contract *work.PlanContract
	if b.Work != nil {
		for _, at := range b.Work.Attempts() {
			if at.Attempt.Conversation != conversationOf(source) {
				continue
			}
			missionID, taskID, attemptID = at.Mission, at.Task, at.Attempt.ID
			state, err := b.Work.Mission(at.Mission)
			if err != nil {
				return nil, reviews.Run{}, err
			}
			if entry, plan := state.ApprovedTaskPlan(at.Attempt.Grant, at.Task); plan != nil {
				contract = plan.Contract
				if entry.Workspace == "pinned" {
					required = plan.Baseline
				}
			}
			break
		}
	}
	bound := contract != nil && req.Scope == reviews.ScopeChanges
	var heads map[string]string
	if bound {
		members, err := b.planBaseline(ctx, workspace.ID)
		if err != nil {
			return nil, reviews.Run{}, fmt.Errorf("commit the candidate before requesting a Mission changes review: %w", err)
		}
		repos := map[string]string{}
		for _, m := range members {
			repos[m.Name] = m.Repository
		}
		for _, m := range required {
			if repos[m.Name] != m.Repository {
				return nil, reviews.Run{}, fmt.Errorf("required Mission member %q is missing or changed", m.Name)
			}
		}
		heads = map[string]string{}
		for _, m := range members {
			heads[m.Name] = m.Commit
		}
	}
	var scope reviewScope
	if req.TemplateID == "" && !bound {
		scope, err = b.scopeOf(ctx, source, workspace, req)
		if err != nil {
			return nil, reviews.Run{}, err
		}
	}
	e, err := b.OpenCollection(profile.ID, workspace.ID, "", accounts.Options{Permission: "read-only", Model: chosen.model}, "", "")
	if err != nil {
		return nil, reviews.Run{}, err
	}
	if req.TemplateID != "" || bound {
		scope, err = b.captureReviewInput(ctx, conversationOf(e), workspace, req.Compare, heads)
		if err != nil {
			_ = e.Engine.Close()
			return nil, reviews.Run{}, err
		}
	}
	if bound {
		if len(scope.input) != len(heads) {
			_ = e.Engine.Close()
			return nil, reviews.Run{}, errors.New("workspace membership changed during review capture")
		}
	}
	label, authorLabel := agentLabel(profile), agentLabel(source.Profile)
	sourceTitle := b.executionTitle(ctx, source)
	template := ""
	if b.Preferences != nil {
		template = b.Preferences.Snapshot().Review.Template
	}
	title := preset.Title + " · " + scope.label + " · " + label
	if b.History != nil {
		_ = b.History.Rename(ctx, e.ID, title)
	}
	run := reviews.Run{TemplateID: req.TemplateID, Input: scope.input, ReviewerSession: e.ID, ReviewerConversation: conversationOf(e), ReviewerTitle: title, ReviewerLabel: label, ReviewerProvider: profile.Provider, ReviewerProfile: profile.ID,
		SourceSession: source.ID, SourceConversation: conversationOf(source), SourceTitle: sourceTitle, SourceLabel: authorLabel, SourceProvider: source.Profile.Provider,
		Scope: req.Scope, ScopeLabel: scope.label, Turn: req.Turn, Compare: req.Compare, Member: req.Member, State: reviews.RunRunning,
		Detail: label + " is reviewing " + strings.ToLower(scope.label[:1]) + scope.label[1:] + "."}
	if req.PullRequest != nil {
		run.PullRequestURL = req.PullRequest.URL
	}
	if fallback != "" {
		run.Detail += " " + fallback
	}
	run.Fallback, run.Focus = fallback, focus
	run.Mission, run.Task = missionID, taskID
	if bound {
		run.ContractDigest = contract.Digest
		run.MemberHeads = heads
		run.ReviewWorkspace = workspace.ID
		run.ReviewAttempt = attemptID
	}
	workspacePlan := ""
	if contract == nil {
		workspacePlan = b.reviewPlan(workspace.ID)
	}
	brief := reviews.Brief(reviews.BriefInput{Reviewer: label, Author: authorLabel, Conversation: sourceTitle, Goal: b.reviewGoal(source), Plan: workspacePlan,
		Scope: scope.label, Diff: scope.diff, Files: scope.files, Evidence: b.reviewEvidence(source), Template: template, Focus: focus, Guidance: preset.Guidance + reviewCompletionInstructions(req.TemplateID)})
	if contract != nil {
		brief += fmt.Sprintf("\n\n## Approved Mission contract\n\nSpec %s, version %d, digest %s. Record what you verified for each criterion and any gaps.\n", contract.Artifact, contract.Version, contract.Digest)
		for _, criterion := range contract.Criteria {
			brief += "\n> " + strings.ReplaceAll(criterion, "\n", "\n> ") + "\n"
		}
	}
	run.BriefOperation = "review-brief-" + strings.TrimPrefix(e.ID, "e_")
	// The loop: how findings reach the author between rounds, and its limit.
	mode, rounds := reviews.LoopAsk, 0
	if b.Preferences != nil {
		prefs := b.Preferences.Snapshot().Review
		if m, err := reviews.LoopMode(prefs.Loop); err == nil {
			mode = m
		}
		rounds = prefs.MaxRounds
	}
	if req.TemplateID != "" {
		mode = reviews.LoopOff
	}
	reviews.StartLoop(&run, mode, rounds, time.Now().Unix())
	run, err = b.Reviews.CreateRun(workspace.ID, run)
	if err != nil {
		_ = e.Engine.Close()
		if req.TemplateID != "" {
			_ = b.Checkpoints.Delete(ctx, conversationOf(e))
		}
		return nil, reviews.Run{}, err
	}
	origin := &agent.Origin{Kind: agent.OriginReviewBrief, Summary: fmt.Sprintf("Asked %s to review %s's work", label, authorLabel), Detail: scope.label}
	if err = b.sendLazyboxMessage(ctx, e, run.BriefOperation, brief, origin); err != nil {
		run, _ = b.Reviews.UpdateRun(workspace.ID, run.ID, func(r *reviews.Run) error {
			r.State, r.Detail = reviews.RunFailed, "The brief could not be sent to "+label+": "+err.Error()
			return nil
		})
		return e, run, err
	}
	// The source conversation shows the review as a card that follows it live.
	b.recordReviewEvent(source, "review_started", run, reviews.Counts{})
	b.recordReviewEvidence(run, reviews.Counts{}, false)
	return e, run, nil
}

// executionTitle is the name a conversation is shown with.
func (b *Bench) executionTitle(ctx context.Context, e *Execution) string {
	if b.History != nil {
		if title := b.conversationTitle(ctx, e.ID); title != "" {
			return title
		}
	}
	return firstOf(e.Engine.Snapshot().Title, e.Saved.Title)
}

// sendLazyboxMessage delivers text Lazybox wrote (a brief, findings, a
// re-review) as a message to e, recorded in the prompt log as such. origin
// is how the conversation shows it: an event, not the person's words.
func (b *Bench) sendLazyboxMessage(ctx context.Context, e *Execution, operation, text string, origin *agent.Origin) error {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	receipt, err := e.Engine.Apply(ctx, agent.Operation{ID: operation, Kind: agent.Message, Text: text, Origin: origin})
	if err != nil {
		return err
	}
	if receipt.Error != "" {
		return errors.New(receipt.Error)
	}
	b.RecordPrompt(e, operation, text, work.PromptSource{Kind: PromptSourceReview})
	return nil
}

// ReviewEvent is the provider data of a review event in a conversation: the
// review it is about, for the card that shows it.
type ReviewEvent struct {
	Review          string `json:"review"`
	Workspace       string `json:"workspace"`
	ReviewerSession string `json:"reviewerSession"`
	ReviewerLabel   string `json:"reviewerLabel"`
	SourceSession   string `json:"sourceSession"`
	SourceLabel     string `json:"sourceLabel"`
	Scope           string `json:"scope"`
	ScopeLabel      string `json:"scopeLabel"`
	Turn            int    `json:"turn,omitempty"`
	Blockers        int    `json:"blockers"`
	Majors          int    `json:"majors"`
	Minors          int    `json:"minors"`
	Nits            int    `json:"nits"`
	// The review's state ("cancelled", "stopped" once the person ended it),
	// and who ended it and when (Unix seconds).
	State     string `json:"state,omitempty"`
	StoppedBy string `json:"stoppedBy,omitempty"`
	StoppedAt int64  `json:"stoppedAt,omitempty"`
}

// recordReviewEvent puts a review event in the source conversation's newest
// attempt: "review_started" (the card) or "review_finished" (its outcome,
// for a transcript read without the Runner's review record).
func (b *Bench) recordReviewEvent(source *Execution, kind string, run reviews.Run, counts reviews.Counts) {
	head, err := b.latest(source)
	if err != nil || head.Archived || head.Engine.Snapshot().Closed {
		return
	}
	data, _ := json.Marshal(ReviewEvent{Review: run.ID, Workspace: head.CollectionID, ReviewerSession: run.ReviewerSession, ReviewerLabel: run.ReviewerLabel, SourceSession: run.SourceSession, SourceLabel: run.SourceLabel,
		Scope: run.Scope, ScopeLabel: run.ScopeLabel, Turn: run.Turn, Blockers: counts.Blockers, Majors: counts.Majors, Minors: counts.Minors, Nits: counts.Nits,
		State: run.State, StoppedBy: run.StoppedBy, StoppedAt: run.StoppedAt})
	text := run.ReviewerLabel + " is reviewing " + lowerFirst(run.ScopeLabel)
	if kind == "review_finished" {
		switch run.State {
		case reviews.RunCancelled:
			text = "Review cancelled · " + plural(counts.Total(), "finding", "findings") + " kept"
		case reviews.RunStopped:
			text = "Review stopped · " + plural(counts.Total(), "finding", "findings") + " kept"
		default:
			text = run.ReviewerLabel + " reviewed " + lowerFirst(run.ScopeLabel) + " · " + counts.Summary()
		}
	}
	_ = b.Record(head, agent.Event{Type: kind, Text: text, Status: run.State, RequestID: run.ID, Data: data})
}

func lowerFirst(text string) string {
	if text == "" || strings.HasPrefix(text, "PR ") {
		return text
	}
	return strings.ToLower(text[:1]) + text[1:]
}

// recordReviewEvidence records the review on the source's mission task, if
// it is an attempt: pending while it runs, failing while blockers are open.
func (b *Bench) recordReviewEvidence(run reviews.Run, counts reviews.Counts, finished bool) {
	if b.Work == nil || run.SourceConversation == "" {
		return
	}
	ev := work.Evidence{Kind: work.EvidenceReview, Ref: "review:" + run.ID, Title: run.ReviewerLabel + " review of " + lowerFirst(run.ScopeLabel), Outcome: work.OutcomePending, Detail: "running",
		Execution: run.ReviewerSession, Blockers: counts.Blockers, Findings: counts.Total()}
	if finished {
		ev.Detail = counts.Summary()
		ev.Outcome = work.OutcomePass
		if run.State == reviews.RunCancelled || run.State == reviews.RunStopped || run.State == reviews.RunFailed {
			// No completed verdict, unless a blocker was filed.
			ev.Detail, ev.Outcome = run.State+" · "+counts.Summary(), work.OutcomeUnknown
		}
		if counts.Blockers > 0 {
			ev.Outcome = work.OutcomeFail
		}
	}
	if run.ContractDigest != "" {
		ev.ContractDigest, ev.Revisions, ev.Workspace = run.ContractDigest, run.MemberHeads, run.ReviewWorkspace
		if finished {
			members, err := b.planBaseline(b.ctx, run.ReviewWorkspace)
			stale := err != nil || len(members) != len(run.MemberHeads)
			for _, m := range members {
				if run.MemberHeads[m.Name] != m.Commit {
					stale = true
				}
			}
			if stale {
				ev.State = "stale"
				ev.Outcome = work.OutcomeUnknown
				ev.Detail = "Candidate changed or could not be read. Review the current workspace revisions."
			}
		}
	}
	actor := work.Actor{Kind: work.ActorAgent, Role: "reviewer", Conversation: run.ReviewerConversation, Execution: run.ReviewerSession}
	_, _ = b.Work.Record(work.Origin{Conversation: run.SourceConversation, Attempt: run.ReviewAttempt}, actor, ev)
}

// reviewTurnEnded follows reviews when a turn of e ended: a reviewer's turn
// settles its review's counts; a turn that changed files may start a review
// when the person asked for that (Settings → Review).
func (b *Bench) reviewTurnEnded(e *Execution, operation string) {
	if b.Reviews == nil {
		return
	}
	matches := b.Reviews.RunsOf(e.ID, conversationOf(e))
	// A turn that answered the other agent's question is that answer.
	if b.answerAsk(e, operation, matches) {
		return
	}
	for _, m := range matches {
		if m.Role == reviews.RoleReviewer {
			b.reviewerTurnEnded(m, operation, e.Engine.Snapshot().LastMessage)
			return
		}
	}
	b.authorTurnEnded(e, operation, matches)
	b.maybeAutoReview(e, operation, matches)
}

// reviewerTurnEnded records what a reviewer's turn found.
func (b *Bench) reviewerTurnEnded(m reviews.Match, operation, reply string) {
	if m.Run.TemplateID != "" && operation != m.Run.BriefOperation {
		return
	}
	review, err := b.Reviews.Load(m.Workspace)
	if err != nil {
		return
	}
	counts := reviews.CountFindings(review, m.Run.ID)
	first, endedByHand := false, false
	// The round the reviewer just finished, and what the loop does next.
	var action reviews.Action
	var run reviews.Run
	_, err = b.Reviews.Update(m.Workspace, func(rv *reviews.Review) error {
		for _, r := range rv.Runs {
			if r.ID != m.Run.ID {
				continue
			}
			// A review the person ended by hand stays as they left it: the
			// turn its cancel or Stop interrupted is not a round's end.
			if endedByHand = r.State == reviews.RunCancelled || r.State == reviews.RunStopped; endedByHand {
				return reviews.Unchanged
			}
			first = r.State == reviews.RunRunning
		}
		var err error
		if action, err = reviews.RoundEnded(rv, m.Run.ID, time.Now().Unix()); err != nil {
			return err
		}
		for i := range rv.Runs {
			if r := &rv.Runs[i]; r.ID == m.Run.ID {
				r.State, r.UpdatedAt = reviews.RunDone, time.Now().Unix()
				if r.TemplateID != "" {
					if err := b.publishReviewResult(m.Workspace, *r, rv.Comments, reviewCoverage(reply)); err != nil {
						r.State, r.Detail = reviews.RunFailed, "Could not publish review result: "+err.Error()
						action = reviews.Action{}
						run = *r
						break
					}
					r.ResultArtifact = r.ID
				}
				if r.StopReason == "" {
					r.Detail = r.ReviewerLabel + " reviewed " + lowerFirst(r.ScopeLabel) + " · " + counts.Summary() + "."
				}
				run = *r
			}
		}
		return nil
	})
	if err != nil || endedByHand {
		return
	}
	defer b.reviewAct(m.Workspace, run, action)
	if first {
		if source, err := b.Get(run.SourceSession); err == nil {
			b.recordReviewEvent(source, "review_finished", run, counts)
		}
	}
	b.recordReviewEvidence(run, counts, true)
}

// maybeAutoReview starts a review of the turn that just ended when the
// person turned that on, the turn changed files, and no review of this
// conversation is already running.
func (b *Bench) maybeAutoReview(e *Execution, operation string, matches []reviews.Match) {
	if b.Preferences == nil || !b.Preferences.Snapshot().Review.AutoStart || b.Checkpoints == nil || operation == "" || e.CollectionID == "" || agent.IsAssistant(e.Purpose) {
		return
	}
	for _, m := range matches {
		if m.Run.State == reviews.RunRunning || (m.Role == reviews.RoleAuthor && m.Run.Active()) {
			return
		}
	}
	record, err := b.Checkpoints.List(conversationOf(e))
	if err != nil {
		return
	}
	for _, t := range record.Turns {
		if t.OperationID == operation && t.After != nil && t.Files > 0 {
			ctx, cancel := context.WithTimeout(b.ctx, time.Minute)
			defer cancel()
			if _, _, err := b.StartReview(ctx, ReviewRequest{Source: e.ID, Scope: reviews.ScopeTurn, Turn: t.Seq, Auto: true}); err != nil {
				log.Printf("automatic review of turn %d: %v", t.Seq, err)
			}
			return
		}
	}
}

// ReviewRunView is a run as it stands now: a review whose reviewer
// conversation has ended or failed before its turn ended says so.
func (b *Bench) ReviewRunView(run reviews.Run) reviews.Run {
	if run.State != reviews.RunRunning {
		return run
	}
	e, err := b.Get(run.ReviewerSession)
	if err != nil {
		run.State, run.Detail = reviews.RunFailed, "The reviewer conversation is gone."
		return run
	}
	if latest, err := b.latest(e); err == nil {
		e = latest
	}
	s := e.Engine.Snapshot()
	switch {
	case s.Closed:
		run.State, run.Detail = reviews.RunFailed, "The reviewer conversation ended before it finished."
	case s.Fault != "" && !agent.LimitFault(s.Fault) && !agent.TurnFault(s.Fault):
		run.State, run.Detail = reviews.RunFailed, "The reviewer stopped: "+s.Fault+"."
	}
	return run
}

// FindingRequest is one finding from `lazybox review add`.
type FindingRequest struct {
	File      string `json:"file"`
	StartLine int    `json:"startLine"`
	EndLine   int    `json:"endLine"`
	Severity  string `json:"severity"`
	Title     string `json:"title"`
	Body      string `json:"body"`
	Key       string `json:"key"`
}

// AddReviewFinding files a finding from the reviewer execution session. The
// workspace, review and author come from the execution; the lines quoted
// are read from the file as it is, so the finding anchors like a comment.
func (b *Bench) AddReviewFinding(ctx context.Context, session string, req FindingRequest) (reviews.Comment, bool, reviews.Run, error) {
	if b.Reviews == nil || b.Collections == nil {
		return reviews.Comment{}, false, reviews.Run{}, errors.New("reviews are not available on this Runner")
	}
	e, err := b.Get(session)
	if err != nil {
		return reviews.Comment{}, false, reviews.Run{}, err
	}
	m, ok := b.Reviews.ReviewerRun(e.ID, conversationOf(e))
	if !ok || m.Workspace != e.CollectionID {
		return reviews.Comment{}, false, reviews.Run{}, ErrNotReviewer
	}
	workspace, err := b.Collections.Get(m.Workspace)
	if err != nil {
		return reviews.Comment{}, false, reviews.Run{}, err
	}
	member, path, err := workspaceFile(workspace, req.File)
	if err != nil {
		return reviews.Comment{}, false, m.Run, err
	}
	var content []byte
	var exists bool
	var head string
	if len(m.Run.Input) > 0 {
		index := slices.IndexFunc(m.Run.Input, func(input reviews.ReviewInput) bool { return input.Member == member.Name })
		if index < 0 {
			return reviews.Comment{}, false, m.Run, errors.New("the file is outside the captured review input")
		}
		input := m.Run.Input[index]
		content, err = codecheckpoints.ReviewFile(ctx, codecheckpoints.RepoPoint{Path: member.Path, Commit: input.Commit}, path)
		exists, head = err == nil, input.Head
	} else {
		content, exists, err = gitinspect.ReadWorking(member.Path, path)
		head, _ = gitinspect.Git{}.Head(ctx, member.Path)
	}
	if err != nil {
		return reviews.Comment{}, false, m.Run, err
	}
	if !exists {
		return reviews.Comment{}, false, m.Run, fmt.Errorf("%s/%s does not exist in the workspace; findings point at a file as it is now", member.Name, path)
	}
	lines := reviews.Lines(string(content))
	if req.EndLine == 0 {
		req.EndLine = req.StartLine
	}
	if req.StartLine < 1 || req.EndLine < req.StartLine {
		return reviews.Comment{}, false, m.Run, errors.New("--line is a line number or a range such as 12-18")
	}
	if req.StartLine > len(lines) {
		return reviews.Comment{}, false, m.Run, fmt.Errorf("%s/%s has %d lines; line %d is not in it", member.Name, path, len(lines), req.StartLine)
	}
	req.EndLine = min(req.EndLine, len(lines), req.StartLine+59)
	c, updated, err := b.Reviews.AddFinding(m.Workspace, reviews.Comment{Member: member.Name, Path: path, StartLine: req.StartLine, Snippet: lines[req.StartLine-1 : req.EndLine],
		FileHash: reviews.FileHash(content), Head: head, View: "uncommitted", Body: req.Body, Author: reviews.AuthorAgent, AuthorLabel: m.Run.ReviewerLabel, AuthorSession: e.ID,
		Severity: req.Severity, Title: req.Title, ReviewID: m.Run.ID, Key: req.Key})
	return c, updated, m.Run, err
}

// workspaceFile resolves "member/path" (or a path in the only repository, or
// an absolute path inside one) to a workspace repository and a path in it.
func workspaceFile(workspace collections.Collection, file string) (collections.Member, string, error) {
	file = strings.TrimSpace(file)
	if file == "" {
		return collections.Member{}, "", errors.New("--file names the file: <repository>/<path>")
	}
	var ready []collections.Member
	for _, m := range workspace.Members {
		if m.Ready {
			ready = append(ready, m)
		}
	}
	if filepath.IsAbs(file) {
		for _, m := range ready {
			if rel, err := filepath.Rel(m.Path, filepath.Clean(file)); err == nil && filepath.IsLocal(rel) {
				return m, filepath.ToSlash(rel), nil
			}
		}
		return collections.Member{}, "", fmt.Errorf("%s is outside this workspace's repositories", file)
	}
	file = filepath.ToSlash(filepath.Clean(strings.TrimPrefix(file, "./")))
	if name, rest, ok := strings.Cut(file, "/"); ok {
		for _, m := range ready {
			if m.Name == name && filepath.IsLocal(rest) {
				return m, rest, nil
			}
		}
	}
	if len(ready) == 1 && filepath.IsLocal(file) {
		return ready[0], file, nil
	}
	names := make([]string, len(ready))
	for i, m := range ready {
		names[i] = m.Name
	}
	return collections.Member{}, "", fmt.Errorf("%s: start the path with the repository (%s)", file, strings.Join(names, ", "))
}

// ReviewFindingsOf is the review session takes part in (as reviewer, or else
// as the newest author) and its findings.
func (b *Bench) ReviewFindingsOf(session string) (reviews.Run, []reviews.Comment, string, error) {
	if b.Reviews == nil {
		return reviews.Run{}, nil, "", errors.New("reviews are not available on this Runner")
	}
	e, err := b.Get(session)
	if err != nil {
		return reviews.Run{}, nil, "", err
	}
	matches := b.Reviews.RunsOf(e.ID, conversationOf(e))
	if len(matches) == 0 {
		return reviews.Run{}, nil, "", errors.New("this conversation takes part in no review")
	}
	m := matches[0]
	for _, candidate := range matches {
		if candidate.Role == reviews.RoleReviewer {
			m = candidate
			break
		}
	}
	if m.Workspace != e.CollectionID {
		return reviews.Run{}, nil, "", errors.New("that review is in another workspace")
	}
	review, err := b.Reviews.Load(m.Workspace)
	if err != nil {
		return reviews.Run{}, nil, "", err
	}
	run := m.Run
	for _, r := range review.Runs {
		if r.ID == run.ID {
			run = r
		}
	}
	return b.ReviewRunView(run), reviews.Findings(review, run.ID), m.Workspace, nil
}
