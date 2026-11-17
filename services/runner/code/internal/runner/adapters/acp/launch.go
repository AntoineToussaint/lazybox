package acp

import (
	"regexp"
	"strings"
)

// LaunchProblem checks a built-in agent's launch command against what the
// installed program says of itself (help, its `--help` output): each flag
// and subcommand Lazybox starts it with (`--acp`, `acp`) must be named
// there. The launch commands follow the ACP registry, not the installed
// version, and an agent that renamed or dropped its ACP mode would otherwise
// fail only when a conversation starts. The answer is a sentence for
// Settings → Agents, or "" when every word is found. It is a hint, never a
// refusal: help text is free-form, so a missing word can be a false alarm.
func (a Agent) LaunchProblem(help string) string {
	if strings.TrimSpace(help) == "" {
		return ""
	}
	var missing []string
	for _, arg := range a.Args {
		if arg == "" || named(help, arg) {
			continue
		}
		missing = append(missing, arg)
	}
	if len(missing) == 0 {
		return ""
	}
	what := "`" + strings.Join(missing, "`, `") + "`"
	fix := "Update it"
	if a.Install != "" {
		fix += " (`" + a.Install + "`)"
	}
	return a.Title + "'s help does not mention " + what + ", which Lazybox starts it with: this version may not run in ACP mode. " + fix + ", or edit how Lazybox starts it."
}

// named reports whether help names arg as a whole word: a flag beside
// punctuation (`--acp,` `[--acp]` `--acp=`), a subcommand in a list.
func named(help, arg string) bool {
	pattern := `(^|[\s,\[(|/])` + regexp.QuoteMeta(arg) + `($|[\s,=\])|/.:])`
	return regexp.MustCompile(pattern).MatchString(help)
}
