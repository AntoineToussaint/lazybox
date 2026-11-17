package main

import (
	"context"
	"fmt"
	"sort"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Shell completions are generated from the verb table, so a verb, a
// subcommand or a flag cannot be missing from them. Names that live in the
// Runner (conversations, workspaces, actions, hooks) are completed by asking
// it through `lazybox __complete KIND`, which prints "value<TAB>description".

// otherVerbs are the verbs main handles itself, completed by name only.
var otherVerbs = []struct{ name, summary string }{
	{"run", "run a command; the console gets every byte, the agent a compact view"},
	{"log", "publish piped output to the console"},
	{"artifact", "publish and read workspace artifacts"},
	{"process", "check and explain the orchestration process"},
	{"events", "raw journal records of a session (JSON)"},
	{"answer", "answer a pending question (--session --request --answers)"},
	{"interrupt", "interrupt a session's turn (--session)"},
	{"info", "the Runner's info (JSON)"},
}

// completionVerbs lists every verb a script completes, with its summary.
func completionVerbs() [][2]string {
	var out [][2]string
	for _, v := range verbs {
		if v.Summary != "" {
			out = append(out, [2]string{v.Name, v.Summary})
		}
	}
	for _, v := range otherVerbs {
		out = append(out, [2]string{v.name, v.summary})
	}
	return out
}

// flagNames and flagValues split "--until=idle|needs-input".
func flagName(spec string) string { name, _, _ := strings.Cut(spec, "="); return name }
func flagValues(spec string) []string {
	if _, values, ok := strings.Cut(spec, "="); ok {
		return strings.Split(values, "|")
	}
	return nil
}

func (c *cli) completion(args []string) error {
	if c.subcommandHelp(args, "Usage: lazybox completion zsh|bash|fish\n  prints a shell completion script (lazybox link installs it for you)") {
		return nil
	}
	if len(args) != 1 {
		return fail(exitUsage, "usage: lazybox completion zsh|bash|fish")
	}
	script, err := completionScript(args[0])
	if err != nil {
		return err
	}
	_, err = fmt.Fprint(c.stdout, script)
	return err
}

func completionScript(shell string) (string, error) {
	switch shell {
	case "zsh":
		return zshScript(), nil
	case "bash":
		return bashScript(), nil
	case "fish":
		return fishScript(), nil
	}
	return "", fail(exitUsage, "completions exist for zsh, bash and fish")
}

func singleQuote(s string) string { return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'" }

func zshScript() string {
	var b strings.Builder
	b.WriteString("#compdef lazybox\n# lazybox completion for zsh. Install: lazybox completion zsh > \"${fpath[1]}/_lazybox\"  (or: source <(lazybox completion zsh))\n\n")
	b.WriteString("_lazybox_names() {\n  local -a items\n  items=(${(f)\"$(lazybox __complete $1 2>/dev/null | sed 's/:/\\\\:/g; s/\\t/:/')\"})\n  _describe -t $1 $1 items\n}\n\n")
	b.WriteString("_lazybox() {\n  local -a verbs\n  verbs=(\n")
	for _, v := range completionVerbs() {
		fmt.Fprintf(&b, "    %s\n", singleQuote(strings.ReplaceAll(v[0], ":", `\:`)+":"+v[1]))
	}
	b.WriteString("  )\n  if (( CURRENT == 2 )); then\n    _describe -t commands 'lazybox command' verbs\n    return\n  fi\n  local cur=${words[CURRENT]} prev=${words[CURRENT-1]}\n  case ${words[2]} in\n")
	for _, v := range verbs {
		if v.Summary == "" {
			continue
		}
		fmt.Fprintf(&b, "    %s)\n", v.Name)
		for _, f := range v.Flags {
			if values := flagValues(f); len(values) > 0 {
				fmt.Fprintf(&b, "      if [[ $prev == %s ]]; then compadd -- %s; return; fi\n", flagName(f), strings.Join(values, " "))
			}
		}
		fmt.Fprintf(&b, "      if [[ $cur == -* ]]; then compadd -- %s --runner --endpoint; return; fi\n", strings.Join(flagNamesOf(v), " "))
		if len(v.Subs) > 0 {
			fmt.Fprintf(&b, "      if (( CURRENT == 3 )); then compadd -- %s; return; fi\n", strings.Join(v.Subs, " "))
		}
		if v.Complete != "" {
			fmt.Fprintf(&b, "      _lazybox_names %s\n", v.Complete)
		}
		b.WriteString("      ;;\n")
	}
	b.WriteString("    *) _files ;;\n  esac\n}\n\nif [[ $zsh_eval_context[-1] == loadautofunc ]]; then\n  _lazybox \"$@\"\nelse\n  compdef _lazybox lazybox\nfi\n")
	return b.String()
}

func flagNamesOf(v verb) []string {
	names := make([]string, 0, len(v.Flags))
	for _, f := range v.Flags {
		names = append(names, flagName(f))
	}
	return names
}

func bashScript() string {
	var b strings.Builder
	b.WriteString("# lazybox completion for bash. Install: lazybox completion bash > ~/.local/share/bash-completion/completions/lazybox  (or: source <(lazybox completion bash))\n\n")
	b.WriteString("_lazybox() {\n  local cur=${COMP_WORDS[COMP_CWORD]} prev=${COMP_WORDS[COMP_CWORD-1]}\n  if (( COMP_CWORD == 1 )); then\n    COMPREPLY=($(compgen -W ")
	var names []string
	for _, v := range completionVerbs() {
		names = append(names, v[0])
	}
	b.WriteString(singleQuote(strings.Join(names, " ")))
	b.WriteString(" -- \"$cur\"))\n    return\n  fi\n  case ${COMP_WORDS[1]} in\n")
	for _, v := range verbs {
		if v.Summary == "" {
			continue
		}
		fmt.Fprintf(&b, "    %s)\n", v.Name)
		for _, f := range v.Flags {
			if values := flagValues(f); len(values) > 0 {
				fmt.Fprintf(&b, "      if [[ $prev == %s ]]; then COMPREPLY=($(compgen -W '%s' -- \"$cur\")); return; fi\n", flagName(f), strings.Join(values, " "))
			}
		}
		fmt.Fprintf(&b, "      if [[ $cur == -* ]]; then COMPREPLY=($(compgen -W '%s --runner --endpoint' -- \"$cur\")); return; fi\n", strings.Join(flagNamesOf(v), " "))
		if len(v.Subs) > 0 {
			fmt.Fprintf(&b, "      if (( COMP_CWORD == 2 )); then COMPREPLY=($(compgen -W '%s' -- \"$cur\")); return; fi\n", strings.Join(v.Subs, " "))
		}
		if v.Complete != "" {
			fmt.Fprintf(&b, "      COMPREPLY=($(compgen -W \"$(lazybox __complete %s 2>/dev/null | cut -f1)\" -- \"$cur\"))\n", v.Complete)
		}
		b.WriteString("      ;;\n")
	}
	b.WriteString("    *) COMPREPLY=($(compgen -f -- \"$cur\")) ;;\n  esac\n}\ncomplete -F _lazybox lazybox\n")
	return b.String()
}

func fishScript() string {
	var b strings.Builder
	b.WriteString("# lazybox completion for fish. Install: lazybox completion fish > ~/.config/fish/completions/lazybox.fish\n\n")
	b.WriteString("complete -c lazybox -f\n")
	for _, v := range completionVerbs() {
		fmt.Fprintf(&b, "complete -c lazybox -n __fish_use_subcommand -a %s -d %s\n", v[0], singleQuote(v[1]))
	}
	for _, v := range verbs {
		if v.Summary == "" {
			continue
		}
		cond := "__fish_seen_subcommand_from " + v.Name
		for _, f := range append(slicesClone(v.Flags), "--runner", "--endpoint") {
			long := strings.TrimPrefix(flagName(f), "--")
			if values := flagValues(f); len(values) > 0 {
				fmt.Fprintf(&b, "complete -c lazybox -n '%s' -l %s -x -a %s\n", cond, long, singleQuote(strings.Join(values, " ")))
			} else {
				fmt.Fprintf(&b, "complete -c lazybox -n '%s' -l %s\n", cond, long)
			}
		}
		if len(v.Subs) > 0 {
			fmt.Fprintf(&b, "complete -c lazybox -n '%s; and not __fish_seen_subcommand_from %s' -a %s\n", cond, strings.Join(v.Subs, " "), singleQuote(strings.Join(v.Subs, " ")))
		}
		if v.Complete != "" {
			fmt.Fprintf(&b, "complete -c lazybox -n '%s' -a '(lazybox __complete %s 2>/dev/null)'\n", cond, v.Complete)
		}
	}
	return b.String()
}

func slicesClone(s []string) []string { return append([]string(nil), s...) }

// complete prints names for the shell scripts: "value<TAB>description" per line.
// It never fails loudly: a Runner that does not answer completes nothing.
func (c *cli) complete(args []string) error {
	if len(args) != 1 {
		return nil
	}
	kind := args[0]
	switch kind {
	case "panels":
		for _, p := range append([]string{"window"}, panelKinds...) {
			c.out("%s\n", p)
		}
		return nil
	case "shells":
		c.out("zsh\nbash\nfish\n")
		return nil
	case "registry":
		for _, a := range power.Registry().Actions {
			c.out("%s\t%s\n", a.ID, oneLine(a.Title, 60))
		}
		return nil
	case "configfiles":
		c.out("keys\nsettings\nactions\nlayouts/\n")
		return nil
	}
	cl, err := c.dial()
	if err != nil {
		return nil
	}
	ctx, cancel := context.WithTimeout(c.ctx, 2*time.Second)
	defer cancel()
	c.ctx = ctx
	var lines []string
	switch kind {
	case "conversations", "targets", "workspaces":
		k, err := loadCatalog(ctx, cl)
		if err != nil {
			return nil
		}
		if kind != "conversations" {
			for _, w := range k.Workspaces {
				lines = append(lines, w.Id+"\tworkspace "+oneLine(w.Name, 60))
			}
		}
		if kind != "workspaces" {
			for _, v := range k.Conversations {
				st, _ := state(v.Head, c.now())
				lines = append(lines, v.Head.Id+"\t"+oneLine(v.Title(), 60)+" ("+stateLabel(st)+")")
			}
		}
	case "actions", "hooks":
		r, err := cl.GetUserActions(ctx, connect.NewRequest(&pb.GetUserActionsRequest{}))
		if err != nil {
			return nil
		}
		if kind == "actions" {
			for _, a := range r.Msg.Actions {
				lines = append(lines, a.Id+"\t"+oneLine(a.Title, 60))
			}
		} else {
			for _, h := range r.Msg.Hooks {
				lines = append(lines, h.Id+"\ton "+strings.Join(h.On, ", "))
			}
		}
	}
	sort.Strings(lines)
	for _, line := range lines {
		c.out("%s\n", line)
	}
	return nil
}
