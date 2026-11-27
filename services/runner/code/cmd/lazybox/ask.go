package main

import (
	"errors"
	"fmt"
	"strings"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox ask author|reviewer "question"` is a question between the author
// and the reviewer of one review: it goes to the other conversation as a
// labelled message, and the reply comes back here (or, past the wait, as a
// message in the asker's conversation). Lazybox admits only those two.

const askUsage = `Usage:
  lazybox ask author "question"     (as the reviewer)
  lazybox ask reviewer "question"   (as the author)
  lazybox ask <review-id> "question"   (when several reviews are active)
  [--timeout 90s] [--no-wait]`

func (c *cli) askAgent(args []string) error {
	f := c.flags("ask")
	wait := f.Duration("timeout", defaultRequestWait, "how long to wait for the reply")
	noWait := f.Bool("no-wait", false, "ask and return at once; the reply comes later as a message in this conversation")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) < 2 {
		fmt.Fprintln(c.stderr, askUsage)
		return fail(exitUsage, "say whom to ask (author or reviewer) and the question")
	}
	to, question := positional[0], strings.Join(positional[1:], " ")
	if question == "-" {
		var err error
		question, err = c.text([]string{"-"})
		if err != nil {
			return err
		}
	}
	session := c.getenv("LAZYBOX_V2_SESSION_ID")
	if session == "" {
		return fail(exitUsage, "lazybox ask is between the author and the reviewer of a review: run it from one of their conversations")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	deadline := c.now().Add(*wait)
	first := uint32(0)
	if !*noWait {
		first = uint32(min(*wait, 55*time.Second) / time.Second)
	}
	a, err := got(cl.AskAgent(c.ctx, connect.NewRequest(&pb.AskAgentRequest{SessionId: session, To: to, Question: question, TimeoutSeconds: first})))
	if err != nil {
		return err
	}
	if *noWait {
		if c.json {
			return c.printProtoJSON(a)
		}
		c.out("Asked %s (question %s). The reply comes as a message in this conversation.\n", a.AnswererLabel, a.Id)
		return nil
	}
	for a.State == "pending" {
		left := deadline.Sub(c.now())
		if left <= 0 {
			break
		}
		next, err := got(cl.WaitAgentAsk(c.ctx, connect.NewRequest(&pb.WaitAgentAskRequest{AskId: a.Id, TimeoutSeconds: uint32(max(min(left, 55*time.Second)/time.Second, 1))})))
		if err != nil {
			var ce *connect.Error
			if errors.As(err, &ce) && ce.Code() == connect.CodeUnavailable && c.ctx.Err() == nil {
				time.Sleep(min(time.Second, max(left, 0)))
				continue
			}
			return err
		}
		a = next
	}
	if c.json {
		if err := c.printProtoJSON(a); err != nil {
			return err
		}
	}
	switch a.State {
	case "answered":
		if !c.json {
			c.out("%s answered:\n\n%s\n", a.AnswererLabel, a.Answer)
		}
		return nil
	case "expired":
		return fail(exitTimeout, "%s did not reply in time; ask again if you still need it", a.AnswererLabel)
	}
	return fail(exitTimeout, "no reply yet from %s. The question stays with it; its reply will come as a message in this conversation. Continue with what you can, or end your turn and say what you are waiting for", a.AnswererLabel)
}
