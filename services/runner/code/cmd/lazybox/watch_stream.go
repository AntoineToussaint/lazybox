package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"sync"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func (c *cli) follow(cl *client, list []conversation, labelled bool, after uint64, from time.Time, replayAll bool, tail int) error {
	if !c.json && len(list) == 1 {
		c.note("Watching “%s” (%s). Ctrl+C stops.\n", list[0].Title(), list[0].Head.Id)
	}
	var out sync.Mutex
	var wg sync.WaitGroup
	errs := make(chan error, len(list))
	for _, v := range list {
		wg.Add(1)
		go func() {
			defer wg.Done()
			errs <- c.stream(c.ctx, cl, &out, labelled, v, after, from, replayAll, tail)
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			return err
		}
	}
	return nil
}

// stream follows one conversation, across a resume to its next execution.
// History shows as its last tail lines (or everything from --since), then live.
func (c *cli) stream(ctx context.Context, cl *client, out *sync.Mutex, labelled bool, v conversation, after uint64, from time.Time, replayAll bool, tail int) error {
	print := func(line string) {
		out.Lock()
		fmt.Fprintln(c.stdout, line)
		out.Unlock()
	}
	// While replaying, lines wait here; only the last tail are shown.
	replaying := !replayAll
	var history []string
	keep := func(line string) {
		history = append(history, line)
		if n := max(tail, 0); len(history) > n {
			history = history[len(history)-n:]
		}
	}
	t := newTimeline(c, labelled, func(line string) {
		if replaying {
			keep(line)
			return
		}
		print(line)
	})
	// endReplay shows the kept history once the stream has caught up.
	endReplay := func() {
		t.mu.Lock()
		if !replaying {
			t.mu.Unlock()
			return
		}
		t.flushSessionLocked(v.Head.Id)
		replaying = false
		lines := history
		history = nil
		t.mu.Unlock()
		for _, line := range lines {
			print(line)
		}
	}
	streamCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	go func() {
		ticker := time.NewTicker(250 * time.Millisecond)
		defer ticker.Stop()
		for {
			select {
			case <-streamCtx.Done():
				return
			case <-ticker.C:
				t.flushIdle(800 * time.Millisecond)
			}
		}
	}()
	session := v.Head.Id
	for {
		s, err := cl.WatchEvents(streamCtx, connect.NewRequest(&pb.WatchEventsRequest{SessionId: session, After: after}))
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return cl.explain(err)
		}
		for s.Receive() {
			j := s.Msg()
			r := toRecord(j, v, session)
			after = j.Cursor
			caughtUp := j.Retained == 0 || j.Cursor >= j.Retained
			if from.IsZero() || !r.time().Before(from) {
				if c.json {
					r.Replay = replaying
					data, _ := json.Marshal(r)
					if replaying {
						keep(string(data))
					} else {
						print(string(data))
					}
				} else {
					t.add(r)
				}
			}
			if caughtUp {
				endReplay()
			}
		}
		err = s.Err()
		s.Close()
		if ctx.Err() != nil {
			t.flushIdle(0)
			return nil
		}
		if err != nil && !errors.Is(err, context.Canceled) {
			return cl.explain(err)
		}
		// The stream ended: the execution closed. Follow a resume, if any.
		got, err := cl.GetSession(ctx, connect.NewRequest(&pb.SessionRequest{SessionId: session}))
		if err != nil || got.Msg.Session.SuccessorExecutionId == "" {
			endReplay()
			t.flushIdle(0)
			if !c.json {
				print(c.paint(dim, fmt.Sprintf("— “%s” ended —", v.Title())))
			}
			return nil
		}
		session, after = got.Msg.Session.SuccessorExecutionId, 0
	}
}

// followWorkspace follows every live conversation of a workspace, and new ones.
func (c *cli) followWorkspace(cl *client, w *pb.Collection, after uint64, from time.Time, replayAll bool, tail int) error {
	if !c.json {
		c.note("Watching workspace “%s”: every live conversation, and new ones. Ctrl+C stops.\n", w.Name)
	}
	var out sync.Mutex
	following := map[string]bool{}
	var wg sync.WaitGroup
	defer wg.Wait()
	for {
		k, err := loadCatalog(c.ctx, cl)
		if err != nil {
			if c.ctx.Err() != nil {
				return nil
			}
			return err
		}
		for _, v := range k.Conversations {
			if v.Head.CollectionId != w.Id || v.Head.Closed || v.Head.Historical || following[v.Key()] {
				continue
			}
			following[v.Key()] = true
			wg.Add(1)
			go func() {
				defer wg.Done()
				if err := c.stream(c.ctx, cl, &out, true, v, after, from, replayAll, tail); err != nil {
					c.note("%s: %v\n", v.Title(), err)
				}
			}()
		}
		select {
		case <-c.ctx.Done():
			return nil
		case <-time.After(c.workspacePoll()):
		}
	}
}

// workspacePoll is how often a workspace watch looks for new conversations.
func (c *cli) workspacePoll() time.Duration { return max(6*c.poll, 500*time.Millisecond) }
