package agentshell

import (
	"bytes"
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

// Check is what `lazybox` agents reach through the person's shell.
type Check struct {
	// CLI is this Runner's CLI, the one agents should reach.
	CLI string `json:"cli"`
	// Shell is the shell that was probed, the one agents' commands run in.
	Shell string `json:"shell"`
	// Shadow is another lazybox the person's startup files put ahead of the
	// CLI, found without the integration; ShadowIn names the shells that do
	// (`zsh -lc`, …). Empty when nothing shadows it.
	Shadow   string   `json:"shadow,omitempty"`
	ShadowIn []string `json:"shadowIn,omitempty"`
	// Unfixed names the shells that still reach something else with the
	// integration, and what. Empty when every shell reaches the CLI.
	Unfixed []string `json:"unfixed,omitempty"`
	// Problem says why the shell could not be probed at all.
	Problem   string    `json:"problem,omitempty"`
	CheckedAt time.Time `json:"checkedAt"`
}

// Fixed reports whether every probed shell reaches the CLI with the integration.
func (c Check) Fixed() bool { return c.Problem == "" && len(c.Unfixed) == 0 }

// Notice is what the person is told, or "" when nothing needs saying.
func (c Check) Notice() string {
	switch {
	case c.Problem != "":
		return ""
	case len(c.Unfixed) > 0:
		return "Agents' shells reach another lazybox before this Lazybox's (" + strings.Join(c.Unfixed, "; ") + "), even with Lazybox's shell integration. Agents are told to call it by its full path, " + c.CLI + ". To fix it for good, remove or rename the other lazybox, or stop your shell startup files from putting its directory first on PATH."
	case c.Shadow != "":
		return "Your shell startup files put another lazybox first on PATH (" + c.Shadow + ", in " + strings.Join(c.ShadowIn, ", ") + "). Lazybox puts its own back first in every agent and workspace terminal shell, so agents run this Lazybox's lazybox; nothing to do."
	}
	return ""
}

// ProbeShells is the shell agents' commands run in, then bash when that is
// not bash or zsh: Claude Code runs commands in bash then.
func ProbeShells(getenv func(string) string) []string {
	shell := getenv("SHELL")
	if shell == "" || !filepath.IsAbs(shell) {
		shell = "/bin/bash"
		if runtime.GOOS == "darwin" {
			shell = "/bin/zsh"
		}
	}
	shells := []string{shell}
	if base := filepath.Base(shell); base != "zsh" && base != "bash" {
		if bash, err := exec.LookPath("bash"); err == nil {
			shells = append(shells, bash)
		}
	}
	return shells
}

// probeMarker frames the answer so banners a startup file prints cannot corrupt it.
const probeMarker = "__LAZYBOX_PROBE__"

// Probe runs `command -v lazybox` in each shell the way agents' tools run
// commands (`-c`, and `-l -c` as Codex does), once with raw (the agent's
// environment without the integration: what the person's files do) and once
// with integrated (what agents get). Each shell gets a few seconds; a person
// whose startup files hang gets a Problem, not a stuck Runner.
func Probe(ctx context.Context, shells []string, raw, integrated []string, cli string) Check {
	c := Check{CLI: cli, CheckedAt: time.Now().UTC()}
	want := canonical(cli)
	if len(shells) > 0 {
		c.Shell = shells[0]
	}
	var problems []string
	for _, shell := range shells {
		for _, mode := range [][]string{{"-c"}, {"-l", "-c"}} {
			label := filepath.Base(shell) + " " + strings.Join(mode, " ")
			if len(mode) == 2 {
				label = filepath.Base(shell) + " -lc"
			}
			found, err := resolve(ctx, shell, mode, raw)
			if err != nil {
				problems = append(problems, label+": "+err.Error())
				continue
			}
			if found != "" && canonical(found) != want {
				if c.Shadow == "" {
					c.Shadow = found
				}
				c.ShadowIn = append(c.ShadowIn, label)
			}
			found, err = resolve(ctx, shell, mode, integrated)
			if err != nil {
				problems = append(problems, label+": "+err.Error())
				continue
			}
			if canonical(found) != want {
				if found == "" {
					found = "no lazybox at all"
				}
				c.Unfixed = append(c.Unfixed, label+" finds "+found)
			}
		}
	}
	if len(problems) > 0 && len(c.Unfixed) == 0 && c.Shadow == "" {
		c.Problem = "could not run the shell: " + strings.Join(problems, "; ")
	}
	return c
}

func resolve(ctx context.Context, shell string, mode, env []string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 8*time.Second)
	defer cancel()
	script := "printf " + probeMarker + "; command -v lazybox; printf " + probeMarker
	cmd := exec.CommandContext(ctx, shell, append(append([]string{}, mode...), script)...)
	cmd.Env = append(append([]string{}, env...), "TERM=dumb")
	cmd.Stdin = nil
	cmd.Dir = os.TempDir()
	out, err := cmd.Output()
	parts := bytes.Split(out, []byte(probeMarker))
	if len(parts) < 3 {
		if err == nil {
			err = context.DeadlineExceeded
			if ctx.Err() == nil {
				err = errNoAnswer
			}
		}
		return "", err
	}
	return strings.TrimSpace(string(parts[1])), nil
}

type probeError string

func (e probeError) Error() string { return string(e) }

const errNoAnswer = probeError("the shell printed no answer")

func canonical(path string) string {
	if resolved, err := filepath.EvalSymlinks(path); err == nil {
		return resolved
	}
	return filepath.Clean(path)
}
