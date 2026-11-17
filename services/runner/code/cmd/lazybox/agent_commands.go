package main

import (
	"fmt"
	"slices"
	"strings"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Inside an agent, `lazybox ls` and `lazybox stop` are about the agent's own
// commands: the servers and watchers its `lazybox run` moved to the
// background, and anything else it runs. Its credential lists and stops only
// those (transport/auth.go); the conversation catalog is the person's.

// agentSession is the execution this CLI runs in as an agent, when the
// Runner it reached holds that agent's scoped credential.
func (c *cli) agentSession(cl *client) string {
	if cl == nil || !cl.target.Scoped {
		return ""
	}
	return c.getenv("LAZYBOX_V2_SESSION_ID")
}

type jsonCommand struct {
	ID         string   `json:"id"`
	Title      string   `json:"title"`
	State      string   `json:"state"`
	Background bool     `json:"background,omitempty"`
	Ports      []uint32 `json:"ports,omitempty"`
	URLs       []string `json:"urls,omitempty"`
	ExitCode   *int32   `json:"exitCode,omitempty"`
	Signal     string   `json:"signal,omitempty"`
	StartedAt  string   `json:"startedAt,omitempty"`
	OutputPath string   `json:"outputPath,omitempty"`
}

// agentLs lists the agent's own commands: running ones first, then the most
// recent that ended.
func (c *cli) agentLs(cl *client, session string) error {
	list, err := got(cl.ListCommands(c.ctx, connect.NewRequest(&pb.ListCommandsRequest{SessionId: session})))
	if err != nil {
		return err
	}
	commands := list.Commands
	slices.SortStableFunc(commands, func(a, b *pb.CommandStatus) int {
		if a.Done != b.Done {
			if a.Done {
				return 1
			}
			return -1
		}
		return int(b.StartedAtUnixMillis - a.StartedAtUnixMillis)
	})
	running := 0
	for _, cmd := range commands {
		if !cmd.Done {
			running++
		}
	}
	if len(commands) > running+5 {
		commands = commands[:running+5]
	}
	rows := make([]jsonCommand, len(commands))
	for i, cmd := range commands {
		rows[i] = describeCommand(cmd)
	}
	if c.json {
		return c.printJSON(map[string]any{"session": session, "commands": rows})
	}
	if len(rows) == 0 {
		c.out("No commands in this conversation yet. Servers and watchers you start with `lazybox run -- …` show here while they run.\n")
		return nil
	}
	c.out("%s\n", c.paint(bold, "Your commands in this conversation"))
	now := c.now()
	for _, r := range rows {
		icon, colour, detail := "✓", green, ""
		switch r.State {
		case "background":
			icon, colour, detail = "●", cyan, "in the background"
		case "running":
			icon, colour, detail = "●", yellow, "running"
		case "stopped":
			icon, colour, detail = "■", dim, "stopped"
			if r.Signal != "" {
				detail += " (" + r.Signal + ")"
			}
		case "failed":
			icon, colour = "✗", red
			detail = "failed"
			if r.ExitCode != nil {
				detail = fmt.Sprintf("exit %d", *r.ExitCode)
			}
		default:
			detail = "exit 0"
			if r.ExitCode != nil {
				detail = fmt.Sprintf("exit %d", *r.ExitCode)
			}
		}
		if r.State == "background" || r.State == "running" {
			if at, err := time.Parse(time.RFC3339, r.StartedAt); err == nil {
				detail += " · " + now.Sub(at).Round(time.Second).String()
			}
			if len(r.URLs) > 0 {
				detail += " · " + strings.Join(r.URLs, ", ")
			}
		}
		c.out("  %s %s  %-40s %s\n", c.paint(colour, icon), r.ID, oneLine(r.Title, 40), c.paint(dim, detail))
	}
	if running > 0 {
		c.out("%s\n", c.paint(dim, "Stop one: lazybox stop ID"))
	}
	return nil
}

func describeCommand(cmd *pb.CommandStatus) jsonCommand {
	r := jsonCommand{ID: cmd.Id, Title: cmd.Title, Background: cmd.Background, Ports: cmd.Ports, Signal: cmd.Signal, ExitCode: cmd.ExitCode}
	if cmd.Report != nil {
		r.OutputPath = cmd.Report.OutputPath
		if r.ExitCode == nil && cmd.Done && cmd.Report.Filter != "" {
			code := cmd.Report.ExitCode
			r.ExitCode = &code
		}
	}
	if cmd.StartedAtUnixMillis > 0 {
		r.StartedAt = time.UnixMilli(cmd.StartedAtUnixMillis).UTC().Format(time.RFC3339)
	}
	for _, p := range cmd.Ports {
		r.URLs = append(r.URLs, fmt.Sprintf("http://localhost:%d", p))
	}
	switch {
	case !cmd.Done && cmd.Background:
		r.State = "background"
	case !cmd.Done:
		r.State = "running"
	case cmd.Signal != "":
		r.State = "stopped"
	case r.ExitCode != nil && *r.ExitCode != 0, cmd.CaptureError != "":
		r.State = "failed"
	default:
		r.State = "succeeded"
	}
	return r
}

// agentStop stops one of the agent's own commands.
func (c *cli) agentStop(cl *client, id string) error {
	status, err := cl.CancelCommand(c.ctx, connect.NewRequest(&pb.CommandRequest{CommandId: id}))
	if err != nil {
		switch connect.CodeOf(err) {
		case connect.CodeNotFound, connect.CodePermissionDenied:
			return fail(exitNotFound, "no command %q in this conversation (lazybox ls lists yours)", id)
		}
		return cl.explain(err)
	}
	if c.json {
		return c.printJSON(map[string]any{"command": status.Msg.Id, "stopping": !status.Msg.Done})
	}
	if status.Msg.Done {
		c.out("Command %s (%s) had already ended.\n", status.Msg.Id, status.Msg.Title)
		return nil
	}
	c.out("Stopping command %s (%s): SIGTERM now, SIGKILL after 5s if it is still running.\n", status.Msg.Id, status.Msg.Title)
	return nil
}
