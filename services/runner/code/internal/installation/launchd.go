package installation

import (
	"bytes"
	"context"
	"encoding/json"
	"encoding/xml"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Service is the launchd job that runs the installed Runner as a per-user
// agent: started at login, started again if it exits, and never tied to the
// desktop app's lifetime. Its agents, commands and terminals live in session
// hosts outside the job's process group (SESSION_HOSTS.md), so stopping or
// replacing the job leaves them running for the next Runner to re-attach.
type Service struct {
	Label     string
	Program   string
	Arguments []string
	// Environment is the job's whole environment besides what launchd sets
	// (HOME, USER, TMPDIR, SSH_AUTH_SOCK, …).
	Environment      map[string]string
	WorkingDirectory string
	Log              string
	// Version is the release the job runs, recorded as LAZYBOX_VERSION.
	Version string
}

// VersionVariable carries the job's release in its environment, where
// `lazybox service status` and the app read it back from the plist.
const VersionVariable = "LAZYBOX_VERSION"

// Plist renders the job definition.
//
// KeepAlive is unconditional: launchd starts the Runner again whatever made
// it exit, which is also what makes Restart Runner in the app available
// (internal/restart needs a supervisor that keeps that promise). A Runner
// that cannot start is retried every ThrottleInterval seconds, and says why
// in the log. ExitTimeOut is how long a stop waits for the Runner to detach
// its agents before launchd kills it; detaching takes well under a second.
// AbandonProcessGroup stays false: what the Runner runs directly dies with
// it, while session hosts have left its process group and survive.
func (s Service) Plist() ([]byte, error) {
	if !labelPattern.MatchString(s.Label) {
		return nil, fmt.Errorf("invalid launchd label %q", s.Label)
	}
	if s.Program == "" || !strings.HasPrefix(s.Program, "/") {
		return nil, errors.New("the service program must be an absolute path")
	}
	var b bytes.Buffer
	b.WriteString(xml.Header)
	b.WriteString(`<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">` + "\n")
	b.WriteString("<!-- Written by `lazybox service install`. Lazybox rewrites this file on update; edit runner.config.json instead. -->\n")
	b.WriteString("<plist version=\"1.0\">\n<dict>\n")
	key := func(k string) { b.WriteString("\t<key>"); xmlEscape(&b, k); b.WriteString("</key>\n") }
	str := func(indent, v string) {
		b.WriteString(indent + "<string>")
		xmlEscape(&b, v)
		b.WriteString("</string>\n")
	}
	key("Label")
	str("\t", s.Label)
	key("ProgramArguments")
	b.WriteString("\t<array>\n")
	for _, a := range append([]string{s.Program}, s.Arguments...) {
		str("\t\t", a)
	}
	b.WriteString("\t</array>\n")
	env := map[string]string{}
	for k, v := range s.Environment {
		env[k] = v
	}
	if s.Version != "" {
		env[VersionVariable] = s.Version
	}
	if len(env) > 0 {
		key("EnvironmentVariables")
		b.WriteString("\t<dict>\n")
		names := make([]string, 0, len(env))
		for k := range env {
			names = append(names, k)
		}
		sort.Strings(names)
		for _, k := range names {
			b.WriteString("\t")
			key(k)
			str("\t\t", env[k])
		}
		b.WriteString("\t</dict>\n")
	}
	if s.WorkingDirectory != "" {
		key("WorkingDirectory")
		str("\t", s.WorkingDirectory)
	}
	if s.Log != "" {
		key("StandardOutPath")
		str("\t", s.Log)
		key("StandardErrorPath")
		str("\t", s.Log)
	}
	key("RunAtLoad")
	b.WriteString("\t<true/>\n")
	key("KeepAlive")
	b.WriteString("\t<true/>\n")
	key("ThrottleInterval")
	b.WriteString("\t<integer>10</integer>\n")
	key("ExitTimeOut")
	b.WriteString("\t<integer>30</integer>\n")
	b.WriteString("</dict>\n</plist>\n")
	return b.Bytes(), nil
}

func xmlEscape(b *bytes.Buffer, s string) { _ = xml.EscapeText(b, []byte(s)) }

// Installed is what the plist on disk says: the program and the release it runs.
type Installed struct {
	Program string
	Version string
	Env     map[string]string
}

// ReadPlist reads a job definition back with plutil, the system's own parser.
func ReadPlist(ctx context.Context, path string) (Installed, error) {
	out, err := exec.CommandContext(ctx, "/usr/bin/plutil", "-convert", "json", "-o", "-", path).Output()
	if err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) {
			return Installed{}, fmt.Errorf("%s is not a readable launchd job: %s", path, strings.TrimSpace(string(exit.Stderr)))
		}
		return Installed{}, err
	}
	var doc struct {
		ProgramArguments     []string          `json:"ProgramArguments"`
		EnvironmentVariables map[string]string `json:"EnvironmentVariables"`
	}
	if err = json.Unmarshal(out, &doc); err != nil {
		return Installed{}, err
	}
	i := Installed{Env: doc.EnvironmentVariables, Version: doc.EnvironmentVariables[VersionVariable]}
	if len(doc.ProgramArguments) > 0 {
		i.Program = doc.ProgramArguments[0]
	}
	return i, nil
}

// Job is launchd's view of a loaded job.
type Job struct {
	Loaded   bool
	Running  bool
	PID      int
	LastExit string
}

// Launchd drives launchctl in this person's GUI domain. Run is replaceable so
// tests never touch the real launchd.
type Launchd struct {
	Domain string
	Run    func(ctx context.Context, args ...string) ([]byte, error)
}

// UserLaunchd is the current user's GUI domain (gui/<uid>), where per-user
// agents in ~/Library/LaunchAgents load.
func UserLaunchd() Launchd {
	return Launchd{Domain: "gui/" + strconv.Itoa(os.Getuid()), Run: func(ctx context.Context, args ...string) ([]byte, error) {
		return exec.CommandContext(ctx, "/bin/launchctl", args...).CombinedOutput()
	}}
}

var (
	pidLine  = regexp.MustCompile(`(?m)^\s*pid = (\d+)\s*$`)
	exitLine = regexp.MustCompile(`(?m)^\s*last exit code = (.+?)\s*$`)
	runLine  = regexp.MustCompile(`(?m)^\s*state = (\S+)\s*$`)
)

// Print reports whether the job is loaded and its process.
func (l Launchd) Print(ctx context.Context, label string) (Job, error) {
	out, err := l.Run(ctx, "print", l.Domain+"/"+label)
	if err != nil {
		// launchctl exits 113 ("Could not find service") for a job that is not loaded.
		if bytes.Contains(out, []byte("Could not find service")) || bytes.Contains(out, []byte("could not find service")) {
			return Job{}, nil
		}
		return Job{}, fmt.Errorf("launchctl print: %v: %s", err, strings.TrimSpace(string(out)))
	}
	j := Job{Loaded: true}
	if m := runLine.FindSubmatch(out); m != nil {
		j.Running = string(m[1]) == "running"
	}
	if m := pidLine.FindSubmatch(out); m != nil {
		j.PID, _ = strconv.Atoi(string(m[1]))
	}
	if m := exitLine.FindSubmatch(out); m != nil {
		j.LastExit = string(m[1])
	}
	return j, nil
}

// Bootstrap loads the plist; RunAtLoad starts the Runner. launchd may still
// be removing a job just booted out under the same label, so a busy answer
// is retried for a few seconds.
func (l Launchd) Bootstrap(ctx context.Context, plist string) error {
	deadline := time.Now().Add(15 * time.Second)
	for {
		out, err := l.Run(ctx, "bootstrap", l.Domain, plist)
		if err == nil {
			return nil
		}
		text := strings.TrimSpace(string(out))
		// "Bootstrap failed: 5: Input/output error" while the previous instance unloads;
		// 37 "Operation already in progress".
		retry := strings.Contains(text, ": 5:") || strings.Contains(text, ": 37:") || strings.Contains(text, "in progress")
		if !retry || time.Now().After(deadline) {
			return fmt.Errorf("launchctl bootstrap: %v: %s", err, text)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(300 * time.Millisecond):
		}
	}
}

// Bootout unloads the job, stopping the Runner (SIGTERM, then SIGKILL after
// ExitTimeOut). A job that is not loaded is not an error.
func (l Launchd) Bootout(ctx context.Context, label string) error {
	out, err := l.Run(ctx, "bootout", l.Domain+"/"+label)
	if err == nil {
		return nil
	}
	text := string(out)
	if strings.Contains(text, "No such process") || strings.Contains(text, "Could not find service") || strings.Contains(text, ": 3:") || strings.Contains(text, ": 113:") {
		return nil
	}
	return fmt.Errorf("launchctl bootout: %v: %s", err, strings.TrimSpace(text))
}

// Kickstart restarts the running job (-k kills the current instance first);
// launchd starts the new one at once.
func (l Launchd) Kickstart(ctx context.Context, label string) error {
	out, err := l.Run(ctx, "kickstart", "-k", l.Domain+"/"+label)
	if err != nil {
		return fmt.Errorf("launchctl kickstart: %v: %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}
