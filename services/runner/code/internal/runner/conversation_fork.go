package runner

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"os"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

type conversationFork struct {
	id, source, profileBinding string
	thread, turn, operation    string
	cursor                     uint64
	history                    []agent.Event
}

// ForkConversation starts a separate Codex conversation from a completed boundary.
func (b *Bench) ForkConversation(ctx context.Context, source, after string, before uint64, checkpointFiles bool, requestKey string) (*Execution, error) {
	if source == "" || (after == "") == (before == 0) {
		return nil, errors.New("select one conversation boundary")
	}
	if requestKey == "" || len(requestKey) > 128 {
		return nil, errors.New("fork request key required (max 128 bytes)")
	}
	// Both the conversation and worktree provisioning are keyed by the request.
	// A client can retry after a lost response without making another branch.
	sum := sha256.Sum256([]byte(fmt.Sprintf("fork:%q:%q:%d:%t:%q", source, after, before, checkpointFiles, requestKey)))
	id := fmt.Sprintf("%x", sum[:16])
	b.forkMu.Lock()
	defer b.forkMu.Unlock()
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if existing, err := b.History.Get(ctx, id); err == nil {
		if existing.ForkedFrom != source || existing.Snapshot.Reference == "" {
			return nil, errors.New("the previous fork failed to start; open a new fork to try again")
		}
		return b.Get(id)
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	e, err := b.Get(source)
	if err != nil {
		return nil, err
	}
	state := e.Engine.Snapshot()
	if e.Saved.Provider != "codex" || agent.IsAssistant(e.Purpose) || state.Reference == "" {
		return nil, agent.ErrUnsupported
	}
	timeline, err := b.OpenTimeline(source)
	if err != nil {
		return nil, err
	}
	fork, err := readForkBoundary(ctx, timeline, state.Reference, after, before)
	if err != nil {
		return nil, err
	}
	fork.id, fork.source = id, source
	fork.profileBinding = e.Saved.ProfileBindings[state.Profile]
	options := e.Options
	if options.Permission != "read-only" {
		options.Permission = "ask"
	}
	options.NoRotation = true
	options.RotationProfiles = nil
	cwd, collection := e.Cwd, e.CollectionID
	if checkpointFiles {
		collection, err = b.forkCheckpointWorkspace(ctx, e, id, fork.operation, before != 0)
		if err != nil {
			return nil, err
		}
		cwd = ""
	}
	child, err := b.open(state.Profile, cwd, "", options, b.GatewayID(e), "", "", collection, e.Purpose, "", false, fork)
	if err != nil && checkpointFiles {
		return nil, fmt.Errorf("checkpoint workspace %s was created, but the conversation could not start: %w", collection, err)
	}
	return child, err
}

func (b *Bench) forkCheckpointWorkspace(ctx context.Context, e *Execution, key, operation string, before bool) (string, error) {
	if b.Collections == nil || e.CollectionID == "" || operation == "" {
		return "", errors.New("a workspace checkpoint is unavailable for this turn")
	}
	record, err := b.CodeCheckpoints(e.ID)
	if err != nil {
		return "", err
	}
	var point *codecheckpoints.Point
	for i := range record.Turns {
		turn := &record.Turns[i]
		if turn.OperationID == operation {
			point = turn.After
			if before {
				point = &turn.Before
			}
			break
		}
	}
	if point == nil || len(point.Repos) == 0 {
		return "", errors.New("the selected turn has no retained file checkpoint")
	}
	source, err := b.Collections.Get(e.CollectionID)
	if err != nil {
		return "", err
	}
	spec := collections.Spec{Name: "Fork of " + source.Name}
	if len(spec.Name) > 128 {
		spec.Name = "Conversation fork"
	}
	for _, snapshot := range point.Repos {
		if snapshot.Error != "" || snapshot.Commit == "" || snapshot.SkippedCount > 0 || len(snapshot.Skipped) > 0 {
			return "", fmt.Errorf("file checkpoint for %s is incomplete", snapshot.Member)
		}
		var repository string
		for _, member := range source.Members {
			if member.Ready && member.Name == snapshot.Member && member.Path == snapshot.Path {
				repository = member.RepositoryID
				break
			}
		}
		if repository == "" {
			return "", fmt.Errorf("checkpoint repository %s is unavailable", snapshot.Member)
		}
		spec.Members = append(spec.Members, collections.MemberSpec{Name: snapshot.Member, RepositoryID: repository, BaseRef: snapshot.Commit})
	}
	workspace, err := b.Collections.Create(ctx, key, spec)
	return workspace.ID, err
}

func readForkBoundary(ctx context.Context, timeline *Timeline, thread, after string, before uint64) (conversationFork, error) {
	var fork conversationFork
	var lastCompleted agent.Event
	var replyCursor uint64
	var historyLength, turnStartIndex int
	var acknowledged string
	var active, laterTurn, unknownHistory bool
	prompts := map[string]agent.Event{}
	for cursor, end := uint64(0), timeline.Retained(); cursor < end; {
		record, err := timeline.Next(ctx, cursor)
		if err != nil {
			return fork, err
		}
		cursor = record.Sequence
		var event agent.Event
		if err := json.Unmarshal(record.Value, &event); err != nil {
			return fork, err
		}
		if cursor == before {
			if event.OperationID == "" || !turnStart(record.Value) || active || laterTurn || unknownHistory {
				break
			}
			var prompt struct {
				ImageCount int `json:"imageCount"`
				Data       struct {
					Files []json.RawMessage `json:"files"`
				} `json:"data"`
			}
			if err := json.Unmarshal(record.Value, &prompt); err != nil {
				return fork, err
			}
			if prompt.ImageCount > 0 || len(prompt.Data.Files) > 0 {
				return fork, errors.New("edit and retry requires a prompt without attachments")
			}
			if lastCompleted.SessionID == thread {
				fork.thread, fork.turn = thread, lastCompleted.TurnID
			}
			fork.cursor, fork.history = cursor, fork.history[:historyLength]
			fork.operation = event.OperationID
			return fork, nil
		}
		if event.AgentID != "" && event.AgentID != "root" {
			continue
		}
		switch event.Type {
		case "operation_accepted":
			if turnStart(record.Value) {
				prompt := agent.Event{Type: event.Type, Text: event.Text}
				if event.OperationID == "" {
					fork.history = append(fork.history, prompt)
				} else {
					laterTurn = true
					prompts[event.OperationID] = prompt
				}
			}
		case "receipt":
			var delivery struct {
				Receipt agent.Receipt `json:"receipt"`
			}
			if err := json.Unmarshal(record.Value, &delivery); err != nil {
				return fork, err
			}
			if prompt, found := prompts[delivery.Receipt.ID]; found {
				if delivery.Receipt.Delivery == "acknowledged" || delivery.Receipt.Delivery == "delivered" {
					if event.Status == "steered" {
						fork.history = append(fork.history, prompt)
					} else {
						fork.history = append(fork.history, agent.Event{})
						copy(fork.history[turnStartIndex+1:], fork.history[turnStartIndex:])
						fork.history[turnStartIndex] = prompt
						acknowledged = delivery.Receipt.ID
					}
				}
				delete(prompts, delivery.Receipt.ID)
			}
		case "message_delta":
			if replyCursor == 0 && event.TurnID == after && event.Text != "" {
				replyCursor = cursor
			}
			fork.history = append(fork.history, agent.Event{Type: event.Type, Text: event.Text})
		case "turn_started":
			active = true
		case "turn_ended":
			operation := event.OperationID
			if operation == "" {
				operation = acknowledged
			}
			if prompt, found := prompts[operation]; found {
				fork.history = append(fork.history, agent.Event{})
				copy(fork.history[turnStartIndex+1:], fork.history[turnStartIndex:])
				fork.history[turnStartIndex] = prompt
				delete(prompts, operation)
			}
			active = false
			laterTurn = false
			fork.history = append(fork.history, agent.Event{Type: event.Type, Status: event.Status})
			turnStartIndex, acknowledged = len(fork.history), ""
			unknownHistory = event.SessionID != thread || event.Status != "completed" || event.TurnID == ""
			if unknownHistory {
				continue
			}
			lastCompleted = event
			historyLength = len(fork.history)
			if after == event.TurnID {
				fork.thread, fork.turn, fork.cursor, fork.operation = thread, after, replyCursor, operation
				return fork, nil
			}
		}
	}
	return conversationFork{}, errors.New("this message has no supported completed Codex boundary")
}
