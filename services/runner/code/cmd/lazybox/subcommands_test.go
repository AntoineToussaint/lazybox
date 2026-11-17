package main

import (
	"context"
	"strings"
	"testing"
	"time"
)

// A verb with subcommands, run bare or with help, prints its usage on stdout
// and exits 0, without a Runner: an agent that runs `lazybox agents` to find
// out what it does gets the answer, not exit 2 and a usage dump on stderr.
func TestBareVerbsPrintHelpAndSucceed(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	home := t.TempDir()
	for _, verb := range []string{"agents", "conversation", "workspace", "review", "mission", "lease", "extension", "completion"} {
		for _, args := range [][]string{{verb}, {verb, "help"}, {verb, "--help"}, {verb, "-h"}} {
			out := runPower(ctx, t, home, map[string]string{}, "", args...)
			// A verb that parses flags first answers -h and --help as the flag package does (on stderr).
			text := out.stdout
			if args[len(args)-1] != verb && args[len(args)-1] != "help" {
				text += out.stderr
			}
			if out.code != 0 || !strings.Contains(strings.ToLower(text), "usage") {
				t.Errorf("%v: %+v", args, out)
			}
		}
	}
	// What the agent in the field ran: its usage names how to start a conversation.
	if out := runPower(ctx, t, home, map[string]string{}, "", "agents"); !strings.Contains(out.stdout, "lazybox conversation new") {
		t.Errorf("agents help does not say how to start a conversation: %s", out.stdout)
	}
	// An unknown subcommand is still a usage error.
	if out := runPower(ctx, t, home, map[string]string{}, "", "agents", "frobnicate"); out.code != exitUsage {
		t.Errorf("unknown subcommand: %+v", out)
	}
}
