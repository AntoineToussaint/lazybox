package installation

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"os/user"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

// Unit renders the service as a systemd user unit.
//
// Restart=always with no start limit is launchd's KeepAlive: systemd starts
// the Runner again whatever made it exit, which is also what makes Restart
// Runner in the app available (internal/restart). TimeoutStopSec is how long
// a stop waits for the Runner to detach its agents before systemd kills it.
//
// KillMode=process: stopping or restarting the unit signals the Runner alone.
// The default (control-group) kills every process in the unit's cgroup,
// whatever its session, which would take the session hosts with it. Each
// host starts in a transient scope of its own (internal/sessionhost), so it
// is not in that cgroup; KillMode=process keeps that promise for a host that
// could not get a scope. What the Runner runs directly still ends with it:
// processguard's lifetime pipe closes when the Runner exits.
//
// Output goes to the journal (`journalctl --user -u lazybox-runner`).
func (s Service) Unit() ([]byte, error) {
	if !labelPattern.MatchString(s.Label) {
		return nil, fmt.Errorf("invalid service label %q", s.Label)
	}
	if s.Program == "" || !strings.HasPrefix(s.Program, "/") {
		return nil, errors.New("the service program must be an absolute path")
	}
	var b bytes.Buffer
	b.WriteString("# Written by `lazybox service install`. Lazybox rewrites this file on update; edit runner.config.json instead.\n")
	b.WriteString("[Unit]\n")
	description := "Lazybox Runner"
	if s.Label != DefaultLabel {
		description += " (" + s.Label + ")"
	}
	b.WriteString("Description=" + escapeSpecifiers(description) + "\n")
	b.WriteString("StartLimitIntervalSec=0\n\n")
	b.WriteString("[Service]\n")
	b.WriteString("Type=simple\n")
	words := make([]string, 0, len(s.Arguments)+1)
	for _, a := range append([]string{s.Program}, s.Arguments...) {
		words = append(words, quoteExecWord(a))
	}
	b.WriteString("ExecStart=" + strings.Join(words, " ") + "\n")
	if s.WorkingDirectory != "" {
		b.WriteString("WorkingDirectory=" + quoteExecWord(s.WorkingDirectory) + "\n")
	}
	env := map[string]string{}
	for k, v := range s.Environment {
		env[k] = v
	}
	if s.Version != "" {
		env[VersionVariable] = s.Version
	}
	names := make([]string, 0, len(env))
	for k := range env {
		names = append(names, k)
	}
	sort.Strings(names)
	for _, k := range names {
		b.WriteString("Environment=" + quoteEnvironment(k+"="+env[k]) + "\n")
	}
	b.WriteString("Restart=always\n")
	b.WriteString("RestartSec=2\n")
	b.WriteString("TimeoutStopSec=30\n")
	b.WriteString("KillMode=process\n\n")
	b.WriteString("[Install]\n")
	b.WriteString("WantedBy=default.target\n")
	return b.Bytes(), nil
}

// escapeSpecifiers doubles %, which systemd otherwise expands (%h, %u, …).
func escapeSpecifiers(s string) string { return strings.ReplaceAll(s, "%", "%%") }

var plainWord = regexp.MustCompile(`^[A-Za-z0-9_./:=,+@-]+$`)

// quoteExecWord quotes one ExecStart word: specifiers and $ are escaped, and
// a word with anything but plain characters is double-quoted with C escapes.
func quoteExecWord(s string) string {
	if plainWord.MatchString(s) {
		return s
	}
	return quoteUnitString(strings.ReplaceAll(escapeSpecifiers(s), "$", "$$"))
}

// quoteEnvironment quotes one Environment= assignment. systemd expands
// specifiers there, but not variables, so $ stays as it is.
func quoteEnvironment(assignment string) string {
	return quoteUnitString(escapeSpecifiers(assignment))
}

func quoteUnitString(s string) string {
	var b strings.Builder
	b.WriteByte('"')
	for _, r := range s {
		switch r {
		case '\\', '"':
			b.WriteByte('\\')
			b.WriteRune(r)
		case '\n':
			b.WriteString(`\n`)
		case '\t':
			b.WriteString(`\t`)
		default:
			b.WriteRune(r)
		}
	}
	b.WriteByte('"')
	return b.String()
}

// unitWords splits a unit file value into words as systemd does for the
// values Unit writes: double-quoted words with C escapes, or bare words.
// Doubled % are undone, and doubled $ too when dollars is set (ExecStart
// expands variables; Environment= does not).
func unitWords(value string, dollars bool) ([]string, error) {
	var words []string
	i := 0
	for {
		for i < len(value) && (value[i] == ' ' || value[i] == '\t') {
			i++
		}
		if i >= len(value) {
			return words, nil
		}
		var w strings.Builder
		if value[i] == '"' {
			i++
			closed := false
			for i < len(value) {
				c := value[i]
				if c == '"' {
					closed = true
					i++
					break
				}
				if c == '\\' && i+1 < len(value) {
					i++
					switch value[i] {
					case 'n':
						w.WriteByte('\n')
					case 't':
						w.WriteByte('\t')
					default:
						w.WriteByte(value[i])
					}
					i++
					continue
				}
				w.WriteByte(c)
				i++
			}
			if !closed {
				return nil, errors.New("unterminated quote")
			}
		} else {
			for i < len(value) && value[i] != ' ' && value[i] != '\t' {
				w.WriteByte(value[i])
				i++
			}
		}
		word := strings.ReplaceAll(w.String(), "%%", "%")
		if dollars {
			word = strings.ReplaceAll(word, "$$", "$")
		}
		words = append(words, word)
	}
}

// ReadUnit reads back a unit Unit wrote: its program and environment.
func ReadUnit(path string) (Installed, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Installed{}, err
	}
	i := Installed{Env: map[string]string{}}
	section := ""
	scanner := bufio.NewScanner(bytes.NewReader(data))
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") || strings.HasPrefix(line, ";") {
			continue
		}
		if strings.HasPrefix(line, "[") {
			section = line
			continue
		}
		key, value, ok := strings.Cut(line, "=")
		if !ok || section != "[Service]" {
			continue
		}
		switch strings.TrimSpace(key) {
		case "ExecStart":
			words, err := unitWords(strings.TrimSpace(value), true)
			if err != nil {
				return Installed{}, fmt.Errorf("%s: ExecStart: %w", path, err)
			}
			if len(words) > 0 {
				i.Program = words[0]
			}
		case "Environment":
			words, err := unitWords(strings.TrimSpace(value), false)
			if err != nil {
				return Installed{}, fmt.Errorf("%s: Environment: %w", path, err)
			}
			for _, w := range words {
				if k, v, ok := strings.Cut(w, "="); ok {
					i.Env[k] = v
				}
			}
		}
	}
	if i.Program == "" {
		return Installed{}, fmt.Errorf("%s is not a Lazybox Runner unit: it has no ExecStart", path)
	}
	i.Version = i.Env[VersionVariable]
	return i, nil
}

// Systemd drives `systemctl --user`: the person's own service manager on
// Linux. Run is replaceable so tests never touch the real one.
type Systemd struct {
	// Run runs program (systemctl, loginctl) with args and returns its
	// combined output.
	Run func(ctx context.Context, program string, args ...string) ([]byte, error)
	// User is the account whose linger setting Linger reads.
	User string
}

// UserSystemd is the calling user's systemd instance.
func UserSystemd() Systemd {
	name := os.Getenv("USER")
	if u, err := user.Current(); err == nil {
		name = u.Username
	}
	return Systemd{User: name, Run: func(ctx context.Context, program string, args ...string) ([]byte, error) {
		return exec.CommandContext(ctx, program, args...).CombinedOutput()
	}}
}

func (Systemd) Kind() string                     { return ManagerSystemd }
func (Systemd) DefinitionPath(p Paths) string    { return p.Definition }
func (Systemd) Render(s Service) ([]byte, error) { return s.Unit() }
func (Systemd) ReadDefinition(_ context.Context, path string) (Installed, error) {
	return ReadUnit(path)
}

// ErrNoUserManager means systemctl --user cannot reach this person's systemd
// instance: no login session (sudo -u, su), or a user manager that is not
// running.
var ErrNoUserManager = errors.New("systemd's user manager is not reachable")

func (s Systemd) systemctl(ctx context.Context, args ...string) ([]byte, error) {
	out, err := s.Run(ctx, "systemctl", append([]string{"--user"}, args...)...)
	if err != nil {
		text := strings.TrimSpace(string(out))
		if strings.Contains(text, "Failed to connect to bus") || strings.Contains(text, "Failed to connect to user scope bus") || strings.Contains(text, "XDG_RUNTIME_DIR") {
			return out, fmt.Errorf("%w (%s): log in to this account directly (SSH or a desktop session, not `su` or `sudo -u`), and turn on lingering with `loginctl enable-linger %s` so it keeps running after you log out; or set %s=supervisor to use Lazybox's own supervisor", ErrNoUserManager, text, s.User, ManagerEnvironment)
		}
		return out, fmt.Errorf("systemctl --user %s: %v: %s", strings.Join(args, " "), err, text)
	}
	return out, nil
}

// Status reads `systemctl --user show` for the unit.
func (s Systemd) Status(ctx context.Context, p Paths) (Job, error) {
	out, err := s.systemctl(ctx, "show", p.Unit(), "--property=LoadState,ActiveState,SubState,MainPID,ExecMainCode,ExecMainStatus,UnitFileState")
	if err != nil {
		return Job{}, err
	}
	props := map[string]string{}
	for _, line := range strings.Split(string(out), "\n") {
		if k, v, ok := strings.Cut(strings.TrimSpace(line), "="); ok {
			props[k] = v
		}
	}
	j := Job{}
	active := props["ActiveState"]
	j.Loaded = props["LoadState"] == "loaded" && (active == "active" || active == "activating" || active == "reloading" || active == "deactivating")
	j.PID, _ = strconv.Atoi(props["MainPID"])
	j.Running = active == "active" && j.PID > 0
	// ExecMainCode 1 is CLD_EXITED (ExecMainStatus is the exit status); 2 and
	// 3 are a signal, which ExecMainStatus numbers.
	switch props["ExecMainCode"] {
	case "1":
		j.LastExit = props["ExecMainStatus"]
	case "2", "3":
		j.LastExit = "signal " + props["ExecMainStatus"]
	}
	if !j.Running && active != "" && active != "inactive" {
		j.LastExit = strings.TrimSpace(j.LastExit + " (" + active + "/" + props["SubState"] + ")")
	}
	if props["UnitFileState"] == "enabled" {
		if linger, err := s.Linger(ctx); err == nil && linger {
			j.Detail = "starts at boot (lingering is on)"
		} else {
			j.Detail = "runs while you are logged in; `loginctl enable-linger " + s.User + "` keeps it running after you log out and starts it at boot"
		}
	}
	return j, nil
}

// Linger reports whether systemd keeps this person's user manager, and so the
// Runner, running without a login session and starts it at boot.
func (s Systemd) Linger(ctx context.Context) (bool, error) {
	out, err := s.Run(ctx, "loginctl", "show-user", s.User, "--property=Linger")
	if err != nil && strings.Contains(string(out), "not logged in or lingering") {
		// logind knows a user only while it has a session or lingers: no
		// session here, so it does not linger.
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("loginctl show-user: %v: %s", err, strings.TrimSpace(string(out)))
	}
	return strings.TrimSpace(string(out)) == "Linger=yes", nil
}

// Activate reloads the unit files, enables the unit for every later start of
// the user manager (at login, or at boot with lingering) and starts it.
func (s Systemd) Activate(ctx context.Context, p Paths) error {
	if _, err := s.systemctl(ctx, "daemon-reload"); err != nil {
		return err
	}
	_, err := s.systemctl(ctx, "enable", "--now", p.Unit())
	return err
}

// Deactivate stops the Runner; the unit stays enabled.
func (s Systemd) Deactivate(ctx context.Context, p Paths) error {
	_, err := s.systemctl(ctx, "stop", p.Unit())
	if err != nil && strings.Contains(err.Error(), "not loaded") {
		return nil
	}
	return err
}

func (s Systemd) Restart(ctx context.Context, p Paths) error {
	_, err := s.systemctl(ctx, "restart", p.Unit())
	return err
}

// Remove stops and disables the unit. A unit systemd does not know is not an error.
func (s Systemd) Remove(ctx context.Context, p Paths) error {
	_, err := s.systemctl(ctx, "disable", "--now", p.Unit())
	if err != nil && (strings.Contains(err.Error(), "does not exist") || strings.Contains(err.Error(), "not loaded") || strings.Contains(err.Error(), "No such file")) {
		return nil
	}
	return err
}

// Forget makes systemd drop a unit whose file was removed.
func (s Systemd) Forget(ctx context.Context) error {
	_, err := s.systemctl(ctx, "daemon-reload")
	return err
}

// Logs is the journal: a user unit's output goes there.
func (Systemd) Logs(p Paths) LogSource {
	base := []string{"journalctl", "--user", "--unit", p.Unit(), "--no-pager", "--output", "short-iso"}
	return LogSource{Command: base, Follow: append(append([]string{}, base...), "--follow"), Lines: "--lines"}
}
