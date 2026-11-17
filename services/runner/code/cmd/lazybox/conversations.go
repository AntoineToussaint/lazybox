package main

import (
	"context"
	"fmt"
	"sort"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Attention states, in the order they matter (as the app's inbox orders them).
const (
	stateNeedsInput = "needs-input"
	stateFailed     = "failed"
	stateWorking    = "working"
	stateReview     = "review"
	stateIdle       = "idle"
	stateEnded      = "ended"
)

var stateOrder = map[string]int{stateNeedsInput: 0, stateFailed: 1, stateWorking: 2, stateReview: 3, stateIdle: 4, stateEnded: 5}

// conversation is one conversation: its newest execution (head) and where it is.
type conversation struct {
	Head      *pb.Session
	Workspace *pb.Collection
}

func (c conversation) Key() string {
	if c.Head.ConversationId != "" {
		return c.Head.ConversationId
	}
	return c.Head.Id
}

func (c conversation) Title() string {
	if t := strings.TrimSpace(c.Head.Title); t != "" {
		return t
	}
	return "Untitled conversation"
}

func (c conversation) WorkspaceName() string {
	if c.Workspace == nil {
		return ""
	}
	return c.Workspace.Name
}

// root is the main agent's activity.
func root(s *pb.Session) *pb.AgentActivity {
	for _, a := range s.Agents {
		if a.Id == "root" {
			return a
		}
	}
	for _, a := range s.Agents {
		if a.ParentId == "" {
			return a
		}
	}
	return nil
}

// state is what the conversation asks of the person, with one short reason.
func state(s *pb.Session, now time.Time) (string, string) {
	if len(s.Pending) > 0 {
		return stateNeedsInput, requestLine(s.Pending[0])
	}
	if s.Fault != "" {
		return stateFailed, s.Fault
	}
	r := root(s)
	if r != nil && (r.Status == "failed" || r.LastTurnStatus == "failed") && !s.Busy {
		return stateFailed, "the last turn failed"
	}
	if s.Busy || s.Queued > 0 || s.Status == "running" || s.Status == "switching_profile" {
		if r != nil && r.Phase != "" {
			if r.Tool != "" {
				return stateWorking, r.Phase + ": " + r.Tool
			}
			return stateWorking, r.Phase
		}
		return stateWorking, ""
	}
	if s.Closed || s.Historical {
		return stateEnded, ""
	}
	if r != nil && r.TurnEndedAtUnixMillis > 0 && now.Sub(time.UnixMilli(r.TurnEndedAtUnixMillis)) < 24*time.Hour {
		return stateReview, "turn ended " + ago(now, time.UnixMilli(r.TurnEndedAtUnixMillis))
	}
	return stateIdle, ""
}

// requestLine summarizes a pending approval or question.
func requestLine(e *pb.AgentEvent) string {
	kind := "Approval"
	if strings.Contains(e.Type, "question") {
		kind = "Question"
	}
	if e.Type == "repository_request" {
		kind = "Repository"
	}
	if e.Type == "conversation_request" {
		kind = "Conversation"
	}
	if text := oneLine(e.Text, 120); text != "" {
		return kind + ": " + text
	}
	return kind
}

func oneLine(text string, limit int) string {
	text = strings.Join(strings.Fields(strings.ReplaceAll(text, "\n", " ")), " ")
	if r := []rune(text); len(r) > limit {
		return string(r[:limit-1]) + "…"
	}
	return text
}

func ago(now, then time.Time) string {
	if then.IsZero() {
		return ""
	}
	d := now.Sub(then)
	switch {
	case d < time.Minute:
		return "just now"
	case d < time.Hour:
		return fmt.Sprintf("%dm ago", int(d.Minutes()))
	case d < 48*time.Hour:
		return fmt.Sprintf("%dh ago", int(d.Hours()))
	}
	return fmt.Sprintf("%dd ago", int(d.Hours()/24))
}

// activity is the newest moment the conversation changed.
func activity(s *pb.Session) time.Time {
	at := s.CreatedAtUnixMillis
	for _, a := range s.Agents {
		at = max(at, a.UpdatedAtUnixMillis, a.TurnEndedAtUnixMillis)
	}
	if at == 0 {
		return time.Time{}
	}
	return time.UnixMilli(at)
}

// catalog is everything a Runner lists: workspaces and conversations.
type catalog struct {
	Workspaces    []*pb.Collection
	Conversations []conversation
}

// maxHistory bounds how many executions the CLI reads to list conversations.
const maxHistory = 500

// loadCatalog reads workspaces and every conversation's newest execution.
// Assistant panels (Ask Lazybox) are part of a workspace, not work, and are left out.
func loadCatalog(ctx context.Context, c *client) (catalog, error) {
	collections, err := got(c.ListCollections(ctx, connect.NewRequest(&pb.ListCollectionsRequest{})))
	if err != nil {
		return catalog{}, err
	}
	out := catalog{Workspaces: collections.Collections}
	byID := map[string]*pb.Session{}
	order := []string{}
	add := func(s *pb.Session) {
		if _, seen := byID[s.Id]; !seen {
			order = append(order, s.Id)
		}
		byID[s.Id] = s
	}
	for offset := uint32(0); offset < maxHistory; offset += 100 {
		page, err := got(c.ListHistory(ctx, connect.NewRequest(&pb.ListHistoryRequest{Limit: 100, Offset: offset})))
		if err != nil {
			return catalog{}, err
		}
		for _, s := range page.Sessions {
			add(s)
		}
		if !page.HasMore {
			break
		}
	}
	// Live executions the history page may not reach yet.
	if info, err := c.Info(ctx, connect.NewRequest(&pb.InfoRequest{})); err == nil {
		for _, s := range info.Msg.Sessions {
			add(s)
		}
	}
	workspaces := map[string]*pb.Collection{}
	for _, w := range out.Workspaces {
		workspaces[w.Id] = w
	}
	heads := map[string]*pb.Session{}
	for _, id := range order {
		s := byID[id]
		if agent.IsAssistant(s.Purpose) || (s.SuccessorExecutionId != "" && byID[s.SuccessorExecutionId] != nil) {
			continue
		}
		key := s.ConversationId
		if key == "" {
			key = s.Id
		}
		if current, ok := heads[key]; ok && current.CreatedAtUnixMillis >= s.CreatedAtUnixMillis {
			continue
		}
		heads[key] = s
	}
	for _, s := range heads {
		out.Conversations = append(out.Conversations, conversation{Head: s, Workspace: workspaces[s.CollectionId]})
	}
	sort.SliceStable(out.Conversations, func(i, j int) bool {
		return activity(out.Conversations[i].Head).After(activity(out.Conversations[j].Head))
	})
	return out, nil
}

// findWorkspace resolves a workspace by id, name, or a unique name prefix.
func (k catalog) findWorkspace(ref string) (*pb.Collection, error) {
	ref = strings.TrimSpace(ref)
	for _, w := range k.Workspaces {
		if w.Id == ref {
			return w, nil
		}
	}
	var exact, prefix []*pb.Collection
	for _, w := range k.Workspaces {
		switch {
		case strings.EqualFold(w.Name, ref):
			exact = append(exact, w)
		case strings.HasPrefix(strings.ToLower(w.Name), strings.ToLower(ref)):
			prefix = append(prefix, w)
		}
	}
	for _, set := range [][]*pb.Collection{exact, prefix} {
		switch len(set) {
		case 0:
			continue
		case 1:
			return set[0], nil
		}
		names := make([]string, len(set))
		for i, w := range set {
			names[i] = fmt.Sprintf("%s (%s)", w.Name, w.Id)
		}
		return nil, fail(exitUsage, "%q names several workspaces: %s; use the id", ref, strings.Join(names, ", "))
	}
	return nil, fail(exitNotFound, "no workspace %q (lazybox ls lists them)", ref)
}

// findConversation resolves a conversation by execution or conversation id,
// "workspace/title", exact title, or a unique title or id prefix.
func (k catalog) findConversation(ref string) (conversation, error) {
	ref = strings.TrimSpace(ref)
	if ref == "" {
		return conversation{}, fail(exitUsage, "name a conversation: its id or title (lazybox ls lists them)")
	}
	for _, c := range k.Conversations {
		if c.Head.Id == ref || c.Key() == ref || c.Head.PreviousExecutionId == ref {
			return c, nil
		}
	}
	pool := k.Conversations
	title := ref
	if workspace, rest, ok := strings.Cut(ref, "/"); ok && rest != "" {
		if w, err := k.findWorkspace(workspace); err == nil {
			pool, title = nil, rest
			for _, c := range k.Conversations {
				if c.Head.CollectionId == w.Id {
					pool = append(pool, c)
				}
			}
		}
	}
	lower := strings.ToLower(title)
	var exact, prefix []conversation
	for _, c := range pool {
		t := strings.ToLower(c.Title())
		switch {
		case t == lower:
			exact = append(exact, c)
		case strings.HasPrefix(t, lower) || (len(title) >= 6 && (strings.HasPrefix(c.Head.Id, title) || strings.HasPrefix(c.Key(), title))):
			prefix = append(prefix, c)
		}
	}
	for _, set := range [][]conversation{exact, prefix} {
		switch len(set) {
		case 0:
			continue
		case 1:
			return set[0], nil
		}
		var names []string
		for i, c := range set {
			if i == 5 {
				names = append(names, "…")
				break
			}
			names = append(names, fmt.Sprintf("%q in %s (%s)", c.Title(), orNone(c.WorkspaceName()), c.Head.Id))
		}
		return conversation{}, fail(exitUsage, "%q names several conversations: %s; use workspace/title or the id", ref, strings.Join(names, ", "))
	}
	return conversation{}, fail(exitNotFound, "no conversation %q (lazybox ls lists them)", ref)
}

func orNone(name string) string {
	if name == "" {
		return "no workspace"
	}
	return name
}
