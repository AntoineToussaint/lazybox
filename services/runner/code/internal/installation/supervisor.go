package installation

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// Supervisor is the Manager where there is no service manager to use: a
// container, or WSL without systemd. `lazybox-runner supervise` runs
// detached in its own session and starts the Runner again whenever it exits,
// as launchd and systemd do, so Restart Runner works and a crash is
// recovered. Nothing starts it at boot: after the machine or the container
// restarts, `lazybox service start` does.
//
// Its record is service.json beside the state (the definition), and
// supervisor.json, which the running supervisor keeps up to date: its own
// process and the Runner's, each with its start time, so a process ID the
// system has since reused is never signalled.
type Supervisor struct{}

// SuperviseArgument is the Runner subcommand that supervises a Runner
// (standalone.go): `lazybox-runner supervise <service.json>`.
const SuperviseArgument = "supervise"

func (Supervisor) Kind() string { return ManagerSupervisor }

// DefinitionPath is service.json in the support directory.
func (Supervisor) DefinitionPath(p Paths) string { return filepath.Join(p.Support, "service.json") }

func statusPath(p Paths) string { return filepath.Join(p.Support, "supervisor.json") }

// Render records the service as JSON.
func (Supervisor) Render(s Service) ([]byte, error) {
	if !labelPattern.MatchString(s.Label) {
		return nil, fmt.Errorf("invalid service label %q", s.Label)
	}
	if s.Program == "" || !filepath.IsAbs(s.Program) {
		return nil, errors.New("the service program must be an absolute path")
	}
	data, err := json.MarshalIndent(s, "", "  ")
	return append(data, '\n'), err
}

func readService(path string) (Service, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Service{}, err
	}
	var s Service
	if err = json.Unmarshal(data, &s); err != nil {
		return Service{}, fmt.Errorf("%s is not a Lazybox service record: %w", path, err)
	}
	if !filepath.IsAbs(s.Program) {
		return Service{}, fmt.Errorf("%s names no Runner", path)
	}
	return s, nil
}

func (Supervisor) ReadDefinition(_ context.Context, path string) (Installed, error) {
	s, err := readService(path)
	if err != nil {
		return Installed{}, err
	}
	env := map[string]string{}
	for k, v := range s.Environment {
		env[k] = v
	}
	if s.Version != "" {
		env[VersionVariable] = s.Version
	}
	return Installed{Program: s.Program, Version: s.Version, Env: env}, nil
}

// supervision is supervisor.json: the live supervisor and the Runner it runs.
type supervision struct {
	PID         int    `json:"pid"`
	Start       string `json:"start"`
	Runner      int    `json:"runner,omitempty"`
	RunnerStart string `json:"runnerStart,omitempty"`
	LastExit    string `json:"lastExit,omitempty"`
}

// alive reports whether pid is still the process that started at start.
func alive(pid int, start string) bool {
	if pid <= 0 || start == "" {
		return false
	}
	now, err := processStart(pid)
	return err == nil && now == start
}

func readSupervision(p Paths) (supervision, bool) {
	var s supervision
	data, err := os.ReadFile(statusPath(p))
	if err != nil || json.Unmarshal(data, &s) != nil {
		return s, false
	}
	return s, alive(s.PID, s.Start)
}

// NotAtBoot is what Status says of how a supervised Runner runs.
const NotAtBoot = "not started at boot: there is no systemd here; after the machine or container restarts, run `lazybox service start`"

func (Supervisor) Status(_ context.Context, p Paths) (Job, error) {
	s, ok := readSupervision(p)
	if !ok {
		return Job{LastExit: s.LastExit}, nil
	}
	j := Job{Loaded: true, LastExit: s.LastExit, Detail: NotAtBoot}
	if alive(s.Runner, s.RunnerStart) {
		j.Running, j.PID = true, s.Runner
	}
	return j, nil
}

// Activate starts a supervisor unless one runs. It is detached in a session
// of its own, so closing the terminal or the SSH connection that started it
// does not stop it, and it appends its output and the Runner's to the log.
func (m Supervisor) Activate(ctx context.Context, p Paths) error {
	if _, ok := readSupervision(p); ok {
		return nil
	}
	definition := m.DefinitionPath(p)
	s, err := readService(definition)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(p.Logs, 0o700); err != nil {
		return err
	}
	log, err := os.OpenFile(p.Log(), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return err
	}
	defer log.Close()
	cmd := exec.Command(s.Program, SuperviseArgument, definition, statusPath(p))
	cmd.Dir = s.WorkingDirectory
	cmd.Env = serviceEnvironment(os.Environ(), s)
	cmd.Stdout, cmd.Stderr = log, log
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err = cmd.Start(); err != nil {
		return err
	}
	pid := cmd.Process.Pid
	// Never waited for: it outlives this process. Release the handle so this
	// process does not keep it.
	_ = cmd.Process.Release()
	deadline := time.Now().Add(10 * time.Second)
	for {
		if sv, ok := readSupervision(p); ok && sv.PID == pid {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("the supervisor did not start; see %s", p.Log())
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(50 * time.Millisecond):
		}
	}
}

// signalSupervisor delivers sig to the live supervisor, after checking it is
// still the process its record names.
func signalSupervisor(p Paths, sig syscall.Signal) (supervision, bool, error) {
	s, ok := readSupervision(p)
	if !ok {
		return s, false, nil
	}
	return s, true, syscall.Kill(s.PID, sig)
}

// Deactivate stops the supervisor, which stops the Runner first. Agents keep
// running in their session hosts.
func (Supervisor) Deactivate(ctx context.Context, p Paths) error {
	s, ok, err := signalSupervisor(p, syscall.SIGTERM)
	if !ok || err != nil {
		return err
	}
	deadline := time.Now().Add(supervisorStopTimeout + 5*time.Second)
	for alive(s.PID, s.Start) {
		if time.Now().After(deadline) {
			return fmt.Errorf("the supervisor (pid %d) did not stop", s.PID)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(50 * time.Millisecond):
		}
	}
	return nil
}

// Restart asks the supervisor for a new Runner (SIGHUP), or starts one.
func (m Supervisor) Restart(ctx context.Context, p Paths) error {
	_, ok, err := signalSupervisor(p, syscall.SIGHUP)
	if err != nil || ok {
		return err
	}
	return m.Activate(ctx, p)
}

func (m Supervisor) Remove(ctx context.Context, p Paths) error {
	if err := m.Deactivate(ctx, p); err != nil {
		return err
	}
	if err := os.Remove(statusPath(p)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

func (Supervisor) Logs(p Paths) LogSource { return LogSource{File: p.Log()} }

// serviceEnvironment is the supervisor's and the Runner's environment: the
// one it was started from, without any Lazybox variable (a `lazybox service
// start` typed in an agent's shell carries that agent's credential), plus
// the service's own.
func serviceEnvironment(base []string, s Service) []string {
	var env []string
	for _, entry := range base {
		key, _, _ := strings.Cut(entry, "=")
		if _, own := s.Environment[key]; own || strings.HasPrefix(key, "LAZYBOX_") {
			continue
		}
		env = append(env, entry)
	}
	for k, v := range s.Environment {
		env = append(env, k+"="+v)
	}
	if s.Version != "" {
		env = append(env, VersionVariable+"="+s.Version)
	}
	return env
}

// supervisorStopTimeout is how long a stopping supervisor waits for the
// Runner to detach its agents before it kills it: launchd's ExitTimeOut.
const supervisorStopTimeout = 30 * time.Second

// supervisorThrottle spaces the starts of a Runner that keeps exiting soon
// after it started, as launchd's ThrottleInterval does.
const supervisorThrottle = 10 * time.Second

// Supervise runs the service in definition until it receives SIGTERM or
// SIGINT, starting the Runner again whenever it exits, and keeps status (the
// supervisor.json Status reads) up to date. SIGHUP replaces the Runner.
func Supervise(definition, status string) error {
	s, err := readService(definition)
	if err != nil {
		return err
	}
	self, err := processStart(os.Getpid())
	if err != nil {
		return fmt.Errorf("cannot read this process's start time: %w", err)
	}
	record := supervision{PID: os.Getpid(), Start: self}
	save := func() {
		if data, err := json.Marshal(record); err == nil {
			_ = writeAtomic(status, append(data, '\n'), 0o600)
		}
	}
	save()
	signals := make(chan os.Signal, 4)
	signal.Notify(signals, syscall.SIGTERM, syscall.SIGINT, syscall.SIGHUP)
	stopping := false
	for !stopping {
		cmd := exec.Command(s.Program, s.Arguments...)
		cmd.Dir = s.WorkingDirectory
		cmd.Env = serviceEnvironment(os.Environ(), s)
		cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
		started := time.Now()
		if err = cmd.Start(); err != nil {
			fmt.Fprintf(os.Stderr, "lazybox supervisor: cannot start the Runner: %v\n", err)
			record.Runner, record.RunnerStart, record.LastExit = 0, "", err.Error()
			save()
		} else {
			record.Runner = cmd.Process.Pid
			record.RunnerStart, _ = processStart(record.Runner)
			save()
			exited := make(chan error, 1)
			go func() { exited <- cmd.Wait() }()
			requested := false
		wait:
			for {
				select {
				case sig := <-signals:
					if sig != syscall.SIGHUP {
						stopping = true
					}
					requested = true
					_ = cmd.Process.Signal(syscall.SIGTERM)
					go func() {
						time.Sleep(supervisorStopTimeout)
						_ = cmd.Process.Kill() // A no-op once it exited: the handle is still unreaped or done.
					}()
				case <-exited:
					break wait
				}
			}
			record.Runner, record.RunnerStart = 0, ""
			record.LastExit = describeExit(cmd.ProcessState)
			save()
			if requested || stopping {
				continue
			}
		}
		// It exited by itself: start it again, more slowly if it keeps failing.
		delay := time.Second
		if time.Since(started) < supervisorThrottle {
			delay = supervisorThrottle
		}
		select {
		case sig := <-signals:
			stopping = sig != syscall.SIGHUP
		case <-time.After(delay):
		}
	}
	_ = os.Remove(status)
	return nil
}

func describeExit(state *os.ProcessState) string {
	if state == nil {
		return ""
	}
	if ws, ok := state.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
		return "signal " + ws.Signal().String()
	}
	return strconv.Itoa(state.ExitCode())
}
