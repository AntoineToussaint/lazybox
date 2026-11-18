package terminals

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// The shell's command-end marks are counted with their status, including one
// cut across two chunks of output, and never twice.
func TestCommandEndsAreCountedAcrossChunks(t *testing.T) {
	var s Status
	var tail []byte
	feed := func(chunk string) { tail = scanMarks(&s, append(tail, chunk...), "") }
	feed("fixture shell\r\n\x1b]133;D;0\x07$ ")
	if s.CommandEnds != 1 || s.LastCommandStatus != 0 || tail != nil {
		t.Fatalf("first prompt: %+v %q", s, tail)
	}
	feed("acme login\r\nsigned in\r\n\x1b]133;D;")
	if s.CommandEnds != 1 {
		t.Fatalf("a mark cut short is not counted yet: %+v", s)
	}
	feed("130\x07$ ")
	if s.CommandEnds != 2 || s.LastCommandStatus != 130 {
		t.Fatalf("the rest of the mark completes it: %+v", s)
	}
	feed("\x1b[1mbold\x1b[0m and \x1b]133;D;2\x1b\\$ ")
	if s.CommandEnds != 3 || s.LastCommandStatus != 2 || tail != nil {
		t.Fatalf("ST-terminated mark, colours ignored: %+v %q", s, tail)
	}
}

// The prompt-ready mark (OSC 133;B) says the shell reads input; a command
// starting (133;C) or ending makes it busy again; OSC 7 says where it is,
// and a path cut across chunks is read whole. Idle needs all three: at a
// prompt, in the folder it started in, not exited.
func TestPromptMarksSayWhenTheShellIsIdle(t *testing.T) {
	dir := private(t)
	start := filepath.Join(dir, "a%b c")
	if err := os.MkdirAll(start, 0o700); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(dir, "link")
	if err := os.Symlink(start, link); err != nil {
		t.Fatal(err)
	}
	var s Status
	var tail []byte
	feed := func(chunk string) { tail = scanMarks(&s, append(tail, chunk...), start) }
	feed("Last login\r\n\x1b]133;D;0\x07P> ")
	if s.PromptsReady != 0 || s.AtPrompt || s.Idle() {
		t.Fatalf("a prompt hook runs before the prompt is drawn: not ready yet: %+v", s)
	}
	osc7 := "\x1b]7;file://host" + strings.ReplaceAll(start, "%", "%25") + "\x07"
	feed(osc7[:len(osc7)/2])
	if s.ShellCwd != "" || len(tail) == 0 {
		t.Fatalf("a folder cut short is carried, not read: %+v %q", s, tail)
	}
	feed(osc7[len(osc7)/2:] + "\x1b]133;B\x07")
	if s.PromptsReady != 1 || !s.AtPrompt || s.ShellCwd != start || !s.InStartFolder || !s.Idle() {
		t.Fatalf("at the prompt in its own folder: %+v", s)
	}
	feed("make seed\r\n\x1b]133;C\x07seeding…\r\n")
	if s.AtPrompt || s.Idle() {
		t.Fatalf("a running command is busy: %+v", s)
	}
	feed("\x1b]133;D;0\x07P> \x1b]7;file://host/\x07\x1b]133;B\x07")
	if s.PromptsReady != 2 || !s.AtPrompt || s.InStartFolder || s.Idle() {
		t.Fatalf("at a prompt elsewhere: not idle for its folder: %+v", s)
	}
	feed("\x1b]7;file://host" + strings.ReplaceAll(link, "%", "%25") + "\x07\x1b]133;B\x07")
	if !s.InStartFolder || !s.Idle() {
		t.Fatalf("a link to its folder is its folder: %+v", s)
	}
	s.Done = true
	if s.Idle() {
		t.Fatal("an exited shell is never idle")
	}
}

func TestPromptMarksNeedZshWithTheIntegration(t *testing.T) {
	integration := []string{"ZDOTDIR=/state/shell-integration/zsh", "LAZYBOX_V2_ZDOTDIR=/state/shell-integration/zsh"}
	for _, c := range []struct {
		shell string
		env   []string
		want  bool
	}{
		{"/bin/zsh", integration, true},
		{"/opt/homebrew/bin/zsh", integration, true},
		{"/bin/bash", integration, false},
		{"/opt/homebrew/bin/fish", integration, false},
		{"/bin/zsh", []string{"ZDOTDIR=/home/me/.config/zsh"}, false},
		{"/bin/zsh", nil, false},
	} {
		if got := promptMarks(c.shell, c.env); got != c.want {
			t.Fatalf("promptMarks(%q, %q) = %v, want %v", c.shell, c.env, got, c.want)
		}
	}
}
