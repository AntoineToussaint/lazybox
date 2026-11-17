package main

import (
	"fmt"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox lease` is an agent's advisory claim on the paths its task will
// change (sota-agents.md §6.5). Nothing is locked: an exclusive lease makes
// another task's claim on the same paths fail, and Lazybox records an
// overlap when another task changes them anyway, and tells the orchestrator.

const leaseUsage = `Usage:
  lazybox lease claim <path> [--exclusive] [--ttl 2h] [--task ID] [--reason …]
  lazybox lease release <lease-id|path>
  lazybox lease ls`

func (c *cli) lease(args []string) error {
	if c.subcommandHelp(args, leaseUsage) {
		return nil
	}
	f := c.flags("lease")
	session := f.String("session", "", "who leases (default: this agent)")
	exclusive := f.Bool("exclusive", false, "no other task may lease these paths while it holds")
	ttl := f.Duration("ttl", 2*time.Hour, "how long it holds (at most 24h)")
	task := f.String("task", "", "the task (an orchestrator, for one of its agents); default: your own")
	reason := f.String("reason", "", "why, shown to the person and the other agents")
	positional, err := c.parse(f, args[1:])
	if err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	var out *pb.MissionLeases
	switch args[0] {
	case "claim":
		if len(positional) != 1 {
			return fail(exitUsage, "usage: lazybox lease claim <path> [--exclusive]")
		}
		out, err = got(cl.ClaimLease(c.ctx, connect.NewRequest(&pb.ClaimLeaseRequest{SessionId: me, TaskId: *task, Path: positional[0], Exclusive: *exclusive, TtlSeconds: uint32(*ttl / time.Second), Reason: *reason})))
	case "release":
		if len(positional) != 1 {
			return fail(exitUsage, "usage: lazybox lease release <lease-id|path>")
		}
		out, err = got(cl.ReleaseLease(c.ctx, connect.NewRequest(&pb.ReleaseLeaseRequest{SessionId: me, Lease: positional[0]})))
	case "ls", "list":
		out, err = got(cl.ListLeases(c.ctx, connect.NewRequest(&pb.LeasesRequest{SessionId: me})))
	default:
		fmt.Fprintln(c.stderr, leaseUsage)
		return fail(exitUsage, "unknown lease command %q", args[0])
	}
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	if l := out.Claimed; l != nil {
		c.out("Leased %s (%s) for “%s” until %s (%s).\n", l.Path, l.Mode, l.TaskTitle, time.UnixMilli(l.ExpiresAtUnixMillis).Format("15:04"), l.Id)
	} else if args[0] == "release" {
		c.out("Released.\n")
	}
	if args[0] == "ls" || args[0] == "list" {
		if len(out.Leases) == 0 {
			c.out("No leases in force on this mission.\n")
		}
		for _, l := range out.Leases {
			c.out("  %-28s %-9s %-24s until %s  %s\n", truncate(l.Path, 28), l.Mode, truncate(l.TaskTitle, 24), time.UnixMilli(l.ExpiresAtUnixMillis).Format("15:04"), l.Id)
		}
	}
	return nil
}
