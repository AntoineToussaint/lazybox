package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// Exit codes every power verb uses (README: "Exit codes").
const (
	exitOK          = 0
	exitFailure     = 1
	exitUsage       = 2
	exitNotFound    = 3
	exitDenied      = 4
	exitTimeout     = 5
	exitUnavailable = 6
)

// exitError carries the process's exit status with the message to print.
type exitError struct {
	code int
	err  error
}

func (e exitError) Error() string { return e.err.Error() }
func (e exitError) Unwrap() error { return e.err }

func fail(code int, format string, args ...any) error {
	return exitError{code: code, err: fmt.Errorf(format, args...)}
}

// exitCode is the status main exits with for err.
func exitCode(err error) int {
	var coded exitError
	if errors.As(err, &coded) {
		return coded.code
	}
	return exitFailure
}

// locator names a Runner the `lazybox` CLI can reach from any terminal. The
// Dev and Working launchers write one per environment (scripts/user-config.mjs).
type locator struct {
	Name     string `json:"name"`
	Product  string `json:"product,omitempty"`
	Endpoint string `json:"endpoint"`
	State    string `json:"state,omitempty"`
	// Path is the locator file itself.
	Path string `json:"-"`
}

// configDir is the person's Lazybox configuration directory:
// $XDG_CONFIG_HOME/lazybox, else ~/.config/lazybox.
func configDir(getenv func(string) string) string {
	if base := getenv("XDG_CONFIG_HOME"); base != "" && filepath.IsAbs(base) {
		return filepath.Join(base, "lazybox")
	}
	home := getenv("HOME")
	if home == "" {
		home, _ = os.UserHomeDir()
	}
	return filepath.Join(home, ".config", "lazybox")
}

// readLocators lists the registered Runners, by name.
func readLocators(getenv func(string) string) ([]locator, error) {
	dir := filepath.Join(configDir(getenv), "runners")
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var out []locator
	for _, entry := range entries {
		if entry.IsDir() || filepath.Ext(entry.Name()) != ".json" {
			continue
		}
		path := filepath.Join(dir, entry.Name())
		data, err := os.ReadFile(path)
		if err != nil {
			continue
		}
		var l locator
		if json.Unmarshal(data, &l) != nil || l.Endpoint == "" {
			continue
		}
		if l.Name == "" {
			l.Name = strings.TrimSuffix(entry.Name(), ".json")
		}
		l.Path = path
		out = append(out, l)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out, nil
}

// target is a resolved Runner: its endpoint file and what it holds.
type target struct {
	// Name is the registered Runner's name, or "" for an explicit endpoint.
	Name         string
	EndpointPath string
	URL, Token   string
	// Scoped: the endpoint came from LAZYBOX_V2_ENDPOINT inside an agent, a
	// workspace terminal or a hook, so it holds a scoped credential.
	Scoped bool
}

// scopedEnvironment reports whether this process runs as an agent, in a
// Lazybox workspace terminal, or in a hook: then it uses only the credential
// it was given, never a registered Runner's owner endpoint.
func scopedEnvironment(getenv func(string) string) bool {
	for _, name := range []string{"LAZYBOX_V2_SESSION_ID", "LAZYBOX_V2_COLLECTION_ID", "LAZYBOX_V2_ENDPOINT"} {
		if getenv(name) != "" {
			return true
		}
	}
	return false
}

// resolveTarget finds the Runner a power verb talks to: --endpoint, then
// LAZYBOX_V2_ENDPOINT, then --runner / LAZYBOX_RUNNER, then the registered
// Runners (the installed "lazybox", then "working", when there are several).
func resolveTarget(getenv func(string) string, endpointFlag, runnerFlag string) (target, error) {
	if endpointFlag != "" {
		return loadTarget("", endpointFlag, getenv("LAZYBOX_V2_ENDPOINT") == endpointFlag && scopedEnvironment(getenv))
	}
	name := runnerFlag
	if name == "" {
		if env := getenv("LAZYBOX_V2_ENDPOINT"); env != "" {
			return loadTarget("", env, true)
		}
		name = getenv("LAZYBOX_RUNNER")
	}
	if name == "" && scopedEnvironment(getenv) {
		return target{}, fail(exitUnavailable, "this process runs inside Lazybox (an agent, a workspace terminal or a hook) without a Runner endpoint; owner verbs are not available here")
	}
	locators, err := readLocators(getenv)
	if err != nil {
		return target{}, fail(exitUnavailable, "cannot read the registered Runners: %v", err)
	}
	if name != "" {
		for _, l := range locators {
			if strings.EqualFold(l.Name, name) {
				return loadTarget(l.Name, l.Endpoint, false)
			}
		}
		if strings.ContainsRune(name, os.PathSeparator) {
			return loadTarget("", name, false)
		}
		return target{}, fail(exitNotFound, "no Runner named %q is registered%s", name, knownRunners(locators))
	}
	switch len(locators) {
	case 0:
		// A foreground Runner, including the container, can use the standard
		// state directory without being registered as a user service.
		if paths, err := installation.UserPaths(getenv); err == nil {
			if _, err := os.Stat(paths.Endpoint()); !errors.Is(err, os.ErrNotExist) {
				return loadTarget("", paths.Endpoint(), false)
			}
		}
		return target{}, fail(exitUnavailable, "no Lazybox Runner found: open the Lazybox app (or run `lazybox service install`), start one with make dev or working, or pass --endpoint <state>/endpoint.json")
	case 1:
		return loadTarget(locators[0].Name, locators[0].Endpoint, false)
	}
	// The installed app's Runner first, then Working: the ones a person uses
	// day to day, rather than Dev, which is being changed.
	for _, preferred := range []string{"lazybox", "working"} {
		for _, l := range locators {
			if l.Name == preferred {
				return loadTarget(l.Name, l.Endpoint, false)
			}
		}
	}
	return target{}, fail(exitUsage, "several Runners are registered; choose one with --runner NAME or LAZYBOX_RUNNER%s", knownRunners(locators))
}

func knownRunners(locators []locator) string {
	if len(locators) == 0 {
		return ""
	}
	names := make([]string, len(locators))
	for i, l := range locators {
		names[i] = l.Name
	}
	return " (" + strings.Join(names, ", ") + ")"
}

func loadTarget(name, path string, scoped bool) (target, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		what := "the endpoint file"
		if name != "" {
			what = "Runner " + name + "'s endpoint file"
		}
		return target{}, fail(exitUnavailable, "%s %s does not exist; is the Runner running?", what, path)
	}
	if err != nil {
		return target{}, fail(exitUnavailable, "cannot read the endpoint file %s: %v", path, err)
	}
	var e endpoint
	if err = json.Unmarshal(data, &e); err != nil || e.URL == "" {
		return target{}, fail(exitUnavailable, "the endpoint file %s is not a Runner endpoint", path)
	}
	return target{Name: name, EndpointPath: path, URL: e.URL, Token: e.Token, Scoped: scoped}, nil
}

// client is a power verb's connection to one Runner.
type client struct {
	genconnect.RunnerServiceClient
	target target
}

func dial(t target) *client {
	// The URL is used exactly as the endpoint file writes it: the Runner
	// refuses any other Host (its DNS-rebinding guard).
	return &client{RunnerServiceClient: genconnect.NewRunnerServiceClient(newHTTPClient(t.Token), t.URL), target: t}
}

// explain turns an RPC failure into a message and an exit code a person can act on.
func (c *client) explain(err error) error {
	if err == nil {
		return nil
	}
	var coded exitError
	if errors.As(err, &coded) {
		return err
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return exitError{exitTimeout, err}
	}
	var ce *connect.Error
	if !errors.As(err, &ce) {
		return exitError{exitFailure, err}
	}
	message := ce.Message()
	switch ce.Code() {
	case connect.CodeNotFound:
		return exitError{exitNotFound, errors.New(message)}
	case connect.CodeInvalidArgument:
		return exitError{exitUsage, errors.New(message)}
	case connect.CodePermissionDenied:
		// The Runner's refusal is one actionable line; a scoped credential (an
		// agent's, a terminal's, a hook's) gets no paragraph appended to it.
		return exitError{exitDenied, errors.New(message)}
	case connect.CodeUnauthenticated:
		return fail(exitDenied, "the Runner refused the credential in %s (%s); if the Runner restarted, its endpoint file has a new one", c.target.EndpointPath, message)
	case connect.CodeDeadlineExceeded:
		return exitError{exitTimeout, errors.New(message)}
	case connect.CodeUnimplemented:
		if strings.Contains(message, "404") {
			return fail(exitUnavailable, "the Runner at %s answered 404: it refused the request's Host; use the endpoint URL as written (localhost), not another name or address for it", c.target.URL)
		}
		return fail(exitFailure, "this Runner does not support that yet (%s); update it", message)
	case connect.CodeUnavailable:
		var op *net.OpError
		if errors.As(err, &op) || strings.Contains(message, "connection refused") || strings.Contains(message, "dial") {
			return fail(exitUnavailable, "Runner connection failed at %s (from %s): %s", c.target.URL, c.target.EndpointPath, message)
		}
		return exitError{exitUnavailable, errors.New(message)}
	}
	return exitError{exitFailure, errors.New(message)}
}

// got unwraps an RPC's response. Its error is explained once, where the verb
// returns (powerVerb), with the Runner it came from.
func got[T any](r *connect.Response[T], err error) (*T, error) {
	if err != nil {
		return nil, err
	}
	return r.Msg, nil
}

// probeTimeout bounds a liveness probe (`lazybox runners`) and a completion.
var probeTimeout = 2 * time.Second
