package main

import "fmt"

// A verb with subcommands, run bare or asked for help, prints its usage and
// succeeds: `lazybox agents` is someone (often an agent) finding out what the
// verb does, not a mistake. An unknown subcommand stays a usage error.

// isHelp reports whether arg asks for help rather than naming a subcommand.
func isHelp(arg string) bool {
	switch arg {
	case "help", "-h", "-help", "--help":
		return true
	}
	return false
}

// subcommandHelp prints usage on stdout when args name no subcommand or ask
// for help, and reports whether it did.
func (c *cli) subcommandHelp(args []string, usage string) bool {
	if len(args) > 0 && !isHelp(args[0]) {
		return false
	}
	fmt.Fprintln(c.stdout, usage)
	return true
}
