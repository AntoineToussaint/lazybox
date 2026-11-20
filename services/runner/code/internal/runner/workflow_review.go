package runner

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
	"gopkg.in/yaml.v3"
	"log"
	"regexp"
	"sort"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

// Workflow reviews derive from retained prompt events. Recall history deliberately
// deduplicates repeated text, which would discard the evidence we need here.
type workflowCandidate struct {
	title, body, original, snippetKey, version string
	conversations                              map[string]bool
}

func workflowCandidates(events []work.Event, workspace string) map[string]*workflowCandidate {
	candidates := map[string]*workflowCandidate{}
	previous := map[string]work.PromptPayload{}
	for _, event := range events {
		if event.Type != work.PromptSubmitted || event.Actor.Kind != work.ActorHuman || !work.Known(event) {
			continue
		}
		var p work.PromptPayload
		if json.Unmarshal(event.Payload, &p) != nil || p.Workspace != workspace {
			continue
		}
		prior := previous[p.Conversation]
		previous[p.Conversation] = p
		if p.Source.Kind != work.SourceTyped && p.Source.Kind != work.SourceInbox && p.Source.Kind != work.SourceConsole {
			continue
		}
		text := strings.TrimSpace(p.Text)
		if text == "" {
			continue
		}
		title := "New snippet"
		var original, snippetKey, version string
		lower := strings.ToLower(text)
		switch {
		case strings.Contains(lower, "agents.md") || strings.HasPrefix(lower, "always ") || strings.HasPrefix(lower, "in this repo"):
			title = "Repository instruction for AGENTS.md"
		case prior.Source.Kind == work.SourceSnippet && (strings.HasPrefix(lower, "also ") || strings.HasPrefix(lower, "instead") || strings.HasPrefix(lower, "you forgot") || strings.HasPrefix(lower, "please include")):
			title = "Revision to snippet " + prior.Source.Snippet
			original, snippetKey, version = prior.Text, prior.Source.Snippet, prior.Source.Digest
		}
		key := title + "\n" + strings.Join(strings.Fields(lower), " ")
		if snippetKey != "" {
			key += "\n" + prior.Source.Digest
		}
		c := candidates[key]
		if c == nil {
			c = &workflowCandidate{title: title, body: text, original: original, snippetKey: snippetKey, version: version, conversations: map[string]bool{}}
			candidates[key] = c
		}
		c.conversations[p.Conversation] = true
	}
	return candidates
}

var snippetWords = regexp.MustCompile(`[a-z0-9]+`)

func (c *workflowCandidate) proposal(id string) (string, string) {
	title := c.title
	if c.snippetKey == "" {
		preview := strings.Join(strings.Fields(c.body), " ")
		if utf8.RuneCountInString(preview) > 60 {
			preview = string([]rune(preview)[:60]) + "…"
		}
		title += " · " + preview
	}
	title = strings.Map(func(r rune) rune {
		if unicode.IsControl(r) {
			return ' '
		}
		return r
	}, title)
	if utf8.RuneCountInString(title) > artifacts.MaxTitle {
		title = string([]rune(title)[:artifacts.MaxTitle-1]) + "…"
	}
	if c.title == "Repository instruction for AGENTS.md" {
		return title, "Review the applicable AGENTS.md in the repository where this convention applies. Proposed addition:\n\n" + c.body
	}
	key, body := c.snippetKey, c.body
	if key == "" {
		key = strings.Join(snippetWords.FindAllString(strings.ToLower(c.body), 5), "-")
		if len(key) > 64 {
			key = key[:64]
		}
		if key == "" {
			key = "snippet-" + digest(id)
		}
	} else {
		body = c.original + "\n" + c.body
	}
	pack, _ := yaml.Marshal([]map[string]string{{"key": key, "body": body}})
	// Indented Markdown keeps arbitrary user text inside the code example.
	proposal := "Proposed snippet pack entry:\n\n    " + strings.ReplaceAll(strings.TrimSuffix(string(pack), "\n"), "\n", "\n    ")
	if c.version != "" {
		proposal += "\n\nBased on snippet version " + c.version + ". Review against the current catalog before applying."
	}
	return title, proposal
}

func (b *Bench) ReviewWorkflow(workspace, key, feedback string) ([]string, error) {
	if workspace == "" {
		return nil, fmt.Errorf("choose a workspace")
	}
	if b.Collections == nil {
		return nil, fmt.Errorf("workspaces unavailable")
	}
	if _, err := b.Collections.Get(workspace); err != nil {
		return nil, err
	}
	if b.Prompts == nil || b.Artifacts == nil {
		return nil, fmt.Errorf("workflow review unavailable")
	}
	candidates := workflowCandidates(b.Prompts.Events(), workspace)
	if feedback != "" || key != "" {
		if strings.TrimSpace(feedback) == "" || key == "" || len(feedback) > 16384 {
			return nil, fmt.Errorf("choose a snippet and enter up to 16 KiB of feedback")
		}
		entry, ok := b.snippet(key, "")
		if !ok {
			return nil, fmt.Errorf("reload the snippet catalog before submitting feedback")
		}
		version := digest(entry.Body)
		candidates["feedback\n"+key+"\n"+version+"\n"+feedback] = &workflowCandidate{title: "Feedback on snippet " + key, body: feedback, original: entry.Body, snippetKey: key, version: version}
	}
	keys := make([]string, 0, len(candidates))
	for id, c := range candidates {
		if c.conversations == nil || len(c.conversations) >= 2 {
			keys = append(keys, id)
		}
	}
	sort.Strings(keys)
	var titles []string
	for _, id := range keys {
		c := candidates[id]
		refs := make([]string, 0, len(c.conversations))
		for conversation := range c.conversations {
			refs = append(refs, conversation)
		}
		sort.Strings(refs)
		title, body := c.proposal(id)
		if len(refs) > 0 {
			body += "\n\nSeen in conversations " + strings.Join(refs, ", ")
		}
		digest := sha256.Sum256([]byte(id))
		doc, err := b.Artifacts.PublishReview(workspace, fmt.Sprintf("workflow-%x", digest[:16]), title, body, "", artifacts.Source{Kind: "edit"})
		if err != nil {
			return nil, err
		}
		if doc.Retired == nil {
			titles = append(titles, doc.Title)
		}
	}
	return titles, nil
}

func (b *Bench) watchWorkflowReviews() {
	if b.Collections == nil {
		return
	}
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		ticker := time.NewTicker(time.Hour)
		defer ticker.Stop()
		for {
			for _, collection := range b.Collections.List() {
				if !collection.HourlyWorkflowReview {
					continue
				}
				workspace := collection.ID
				if _, err := b.ReviewWorkflow(workspace, "", ""); err != nil {
					log.Printf("workflow review %s: %v", workspace, err)
				}
			}
			select {
			case <-b.ctx.Done():
				return
			case <-ticker.C:
			}
		}
	}()
}
