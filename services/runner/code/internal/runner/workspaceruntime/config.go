package workspaceruntime

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/url"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"

	"gopkg.in/yaml.v3"
)

// ConfigPath is where a repository commits its runtime configuration,
// relative to the repository root.
const ConfigPath = ".lazybox/workspace.yaml"

// Bounds on what a configuration may hold.
const (
	MaxConfigBytes = 64 << 10
	MaxScriptBytes = 16 << 10
	MaxActions     = 32
	MaxFiles       = 64
	MaxEnv         = 64
	// PortCount is the size of every workspace's port block:
	// LAZYBOX_PORT_0 … LAZYBOX_PORT_9.
	PortCount = 10
	// DefaultArchiveTimeout bounds an archive script that sets no timeout:
	// deleting a workspace waits for it no longer.
	DefaultArchiveTimeout = 120
	MaxTimeout            = 3600
)

// Script is a setup or archive script: a shell script run in the member's
// worktree (or cwd inside it) with the workspace's variables.
type Script struct {
	Run string            `json:"run"`
	Cwd string            `json:"cwd,omitempty"`
	Env map[string]string `json:"env,omitempty"`
	// Timeout, in seconds, after which the script is stopped. Zero: setup
	// runs until it ends; archive uses DefaultArchiveTimeout.
	Timeout int `json:"timeout,omitempty"`
}

// Action is a named run script: a dev server, a test watcher, a worker.
type Action struct {
	Name        string            `json:"name"`
	Description string            `json:"description,omitempty"`
	Run         string            `json:"run"`
	Cwd         string            `json:"cwd,omitempty"`
	Env         map[string]string `json:"env,omitempty"`
	Default     bool              `json:"default,omitempty"`
	// Concurrent false: starting the action stops the same repository's
	// action of the same name in every other workspace (an app that binds a
	// fixed port, a shared database or a single-instance lock).
	Concurrent bool `json:"concurrent"`
	// URL is where the action serves, usually with $LAZYBOX_PORT.
	URL string `json:"url,omitempty"`
	// Port picks which port of the block is this action's LAZYBOX_PORT (0–9).
	Port int `json:"port,omitempty"`
}

// Config is one repository's runtime configuration.
type Config struct {
	Version int               `json:"version"`
	Setup   *Script           `json:"setup,omitempty"`
	Archive *Script           `json:"archive,omitempty"`
	Files   []string          `json:"files,omitempty"`
	Env     map[string]string `json:"env,omitempty"`
	Actions []Action          `json:"actions,omitempty"`
}

// Empty reports whether the configuration defines nothing to run or copy.
func (c Config) Empty() bool {
	return c.Setup == nil && c.Archive == nil && len(c.Files) == 0 && len(c.Env) == 0 && len(c.Actions) == 0
}

// Action finds an action by name.
func (c Config) Action(name string) (Action, bool) {
	for _, a := range c.Actions {
		if a.Name == name {
			return a, true
		}
	}
	return Action{}, false
}

var (
	actionName = regexp.MustCompile(`^[a-z0-9][a-z0-9._-]{0,39}$`)
	envName    = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]{0,127}$`)
)

// Parse decodes and validates a configuration. Errors name the line, and
// unknown keys are errors: a misspelt key must not silently drop a script.
func Parse(data []byte) (Config, error) {
	if len(data) > MaxConfigBytes {
		return Config{}, fmt.Errorf("%s is larger than %d KiB", ConfigPath, MaxConfigBytes>>10)
	}
	if !utf8.Valid(data) {
		return Config{}, errors.New("the configuration is not UTF-8 text")
	}
	var doc yaml.Node
	if err := yaml.Unmarshal(data, &doc); err != nil {
		return Config{}, fmt.Errorf("not valid YAML: %w", err)
	}
	c := Config{Version: 1}
	if len(doc.Content) == 0 {
		return c, nil
	}
	root := doc.Content[0]
	if root.Kind != yaml.MappingNode {
		return Config{}, lineError(root, "the configuration must be a mapping of setup, archive, files, env and actions")
	}
	err := eachKey(root, map[string]func(*yaml.Node) error{
		"version": func(n *yaml.Node) error {
			v, err := intValue(n)
			if err == nil && v != 1 {
				err = lineError(n, "version must be 1")
			}
			c.Version = v
			return err
		},
		"setup":   func(n *yaml.Node) (err error) { c.Setup, err = script(n, "setup"); return },
		"archive": func(n *yaml.Node) (err error) { c.Archive, err = script(n, "archive"); return },
		"files":   func(n *yaml.Node) (err error) { c.Files, err = files(n); return },
		"env":     func(n *yaml.Node) (err error) { c.Env, err = env(n); return },
		"actions": func(n *yaml.Node) (err error) { c.Actions, err = actions(n); return },
	})
	if err != nil {
		return Config{}, err
	}
	return c, nil
}

func lineError(n *yaml.Node, format string, args ...any) error {
	return fmt.Errorf("line %d: %s", n.Line, fmt.Sprintf(format, args...))
}

// eachKey walks a mapping, refusing duplicate and unknown keys.
func eachKey(n *yaml.Node, fields map[string]func(*yaml.Node) error) error {
	if n.Kind != yaml.MappingNode {
		return lineError(n, "expected a mapping")
	}
	seen := map[string]bool{}
	for i := 0; i+1 < len(n.Content); i += 2 {
		key, value := n.Content[i], n.Content[i+1]
		decode, ok := fields[key.Value]
		if !ok {
			known := make([]string, 0, len(fields))
			for name := range fields {
				known = append(known, name)
			}
			sort.Strings(known)
			return lineError(key, "unknown key %q (expected one of %s)", key.Value, strings.Join(known, ", "))
		}
		if seen[key.Value] {
			return lineError(key, "%q is set twice", key.Value)
		}
		seen[key.Value] = true
		if value.Tag == "!!null" {
			continue
		}
		if err := decode(value); err != nil {
			return err
		}
	}
	return nil
}

func stringValue(n *yaml.Node) (string, error) {
	if n.Kind != yaml.ScalarNode || n.Tag == "!!null" {
		return "", lineError(n, "expected text")
	}
	if strings.ContainsRune(n.Value, 0) {
		return "", lineError(n, "text must not contain NUL")
	}
	return n.Value, nil
}

func intValue(n *yaml.Node) (int, error) {
	if n.Kind != yaml.ScalarNode || n.Tag != "!!int" {
		return 0, lineError(n, "expected a whole number")
	}
	v, err := strconv.Atoi(n.Value)
	if err != nil {
		return 0, lineError(n, "expected a whole number")
	}
	return v, nil
}

func boolValue(n *yaml.Node) (bool, error) {
	if n.Kind != yaml.ScalarNode || n.Tag != "!!bool" {
		return false, lineError(n, "expected true or false")
	}
	var v bool
	if err := n.Decode(&v); err != nil {
		return false, lineError(n, "expected true or false")
	}
	return v, nil
}

func run(n *yaml.Node) (string, error) {
	v, err := stringValue(n)
	if err != nil {
		return "", err
	}
	if strings.TrimSpace(v) == "" {
		return "", lineError(n, "run must not be empty")
	}
	if len(v) > MaxScriptBytes {
		return "", lineError(n, "a script is limited to %d KiB", MaxScriptBytes>>10)
	}
	return v, nil
}

// cwd is a directory inside the member's worktree.
func cwd(n *yaml.Node) (string, error) {
	v, err := stringValue(n)
	if err != nil {
		return "", err
	}
	clean := filepath.Clean(v)
	if filepath.IsAbs(v) || !filepath.IsLocal(clean) {
		return "", lineError(n, "cwd must be a folder inside the repository, like packages/web")
	}
	if clean == "." {
		return "", nil
	}
	return filepath.ToSlash(clean), nil
}

func script(n *yaml.Node, what string) (*Script, error) {
	s := &Script{}
	if n.Kind == yaml.ScalarNode {
		v, err := run(n)
		s.Run = v
		return s, err
	}
	err := eachKey(n, map[string]func(*yaml.Node) error{
		"run": func(n *yaml.Node) (err error) { s.Run, err = run(n); return },
		"cwd": func(n *yaml.Node) (err error) { s.Cwd, err = cwd(n); return },
		"env": func(n *yaml.Node) (err error) { s.Env, err = env(n); return },
		"timeout": func(n *yaml.Node) (err error) {
			s.Timeout, err = intValue(n)
			if err == nil && (s.Timeout < 1 || s.Timeout > MaxTimeout) {
				err = lineError(n, "timeout is in seconds, from 1 to %d", MaxTimeout)
			}
			return
		},
	})
	if err == nil && s.Run == "" {
		err = lineError(n, "%s needs run: the script to run", what)
	}
	return s, err
}

func env(n *yaml.Node) (map[string]string, error) {
	out := map[string]string{}
	if n.Kind != yaml.MappingNode {
		return nil, lineError(n, "env is a mapping of NAME: value")
	}
	for i := 0; i+1 < len(n.Content); i += 2 {
		key, value := n.Content[i], n.Content[i+1]
		if !envName.MatchString(key.Value) {
			return nil, lineError(key, "%q is not a variable name", key.Value)
		}
		if strings.HasPrefix(key.Value, "LAZYBOX_") {
			return nil, lineError(key, "%s is set by Lazybox and cannot be overridden", key.Value)
		}
		if _, dup := out[key.Value]; dup {
			return nil, lineError(key, "%q is set twice", key.Value)
		}
		v, err := stringValue(value)
		if err != nil {
			return nil, err
		}
		if len(v) > 4096 {
			return nil, lineError(value, "a value is limited to 4 KiB")
		}
		out[key.Value] = v
	}
	if len(out) > MaxEnv {
		return nil, lineError(n, "at most %d variables", MaxEnv)
	}
	return out, nil
}

func files(n *yaml.Node) ([]string, error) {
	if n.Kind != yaml.SequenceNode {
		return nil, lineError(n, "files is a list of paths, like - .env")
	}
	if len(n.Content) > MaxFiles {
		return nil, lineError(n, "at most %d files", MaxFiles)
	}
	out := []string{}
	seen := map[string]bool{}
	for _, item := range n.Content {
		v, err := stringValue(item)
		if err != nil {
			return nil, err
		}
		clean := filepath.Clean(v)
		if v == "" || filepath.IsAbs(v) || !filepath.IsLocal(clean) || clean == "." {
			return nil, lineError(item, "%q must be a file path inside the repository", v)
		}
		for _, part := range strings.Split(filepath.ToSlash(clean), "/") {
			if part == ".git" {
				return nil, lineError(item, "files never copies from .git")
			}
		}
		if _, err := filepath.Match(clean, ""); err != nil {
			return nil, lineError(item, "%q is not a valid pattern", v)
		}
		clean = filepath.ToSlash(clean)
		if !seen[clean] {
			seen[clean] = true
			out = append(out, clean)
		}
	}
	return out, nil
}

func actions(n *yaml.Node) ([]Action, error) {
	if n.Kind != yaml.MappingNode {
		return nil, lineError(n, "actions is a mapping of name: {run: …}")
	}
	if len(n.Content)/2 > MaxActions {
		return nil, lineError(n, "at most %d actions", MaxActions)
	}
	out := []Action{}
	defaults := 0
	seen := map[string]bool{}
	for i := 0; i+1 < len(n.Content); i += 2 {
		key, value := n.Content[i], n.Content[i+1]
		if !actionName.MatchString(key.Value) {
			return nil, lineError(key, "%q: action names are lowercase letters, digits, '.', '_' or '-' (at most 40)", key.Value)
		}
		if seen[key.Value] {
			return nil, lineError(key, "action %q is defined twice", key.Value)
		}
		seen[key.Value] = true
		a := Action{Name: key.Value, Concurrent: true}
		var err error
		if value.Kind == yaml.ScalarNode {
			a.Run, err = run(value)
		} else {
			err = eachKey(value, map[string]func(*yaml.Node) error{
				"run":         func(n *yaml.Node) (err error) { a.Run, err = run(n); return },
				"description": func(n *yaml.Node) (err error) { a.Description, err = stringValue(n); return },
				"cwd":         func(n *yaml.Node) (err error) { a.Cwd, err = cwd(n); return },
				"env":         func(n *yaml.Node) (err error) { a.Env, err = env(n); return },
				"default":     func(n *yaml.Node) (err error) { a.Default, err = boolValue(n); return },
				"concurrent":  func(n *yaml.Node) (err error) { a.Concurrent, err = boolValue(n); return },
				"url":         func(n *yaml.Node) (err error) { a.URL, err = previewURL(n); return },
				"port": func(n *yaml.Node) (err error) {
					a.Port, err = intValue(n)
					if err == nil && (a.Port < 0 || a.Port >= PortCount) {
						err = lineError(n, "port picks one of the workspace's ports, 0 to %d", PortCount-1)
					}
					return
				},
			})
		}
		if err != nil {
			return nil, err
		}
		if a.Run == "" {
			return nil, lineError(value, "action %q needs run: the command to run", a.Name)
		}
		if len(a.Description) > 200 {
			return nil, lineError(value, "a description is limited to 200 characters")
		}
		if a.Default {
			defaults++
			if defaults > 1 {
				return nil, lineError(key, "only one action can be the default")
			}
		}
		out = append(out, a)
	}
	return out, nil
}

// previewURL accepts an http(s) address on this machine: a preview of
// another host is a bookmark, not a workspace's own server.
func previewURL(n *yaml.Node) (string, error) {
	v, err := stringValue(n)
	if err != nil {
		return "", err
	}
	if _, err := CheckURL(Expand(v, sampleVariables)); err != nil {
		return "", lineError(n, "%v", err)
	}
	return v, nil
}

var sampleVariables = func() map[string]string {
	vars := map[string]string{"LAZYBOX_PORT": "20000"}
	for i := range PortCount {
		vars["LAZYBOX_PORT_"+strconv.Itoa(i)] = strconv.Itoa(20000 + i)
	}
	return vars
}()

// CheckURL accepts an http(s) URL on a loopback host (localhost, *.localhost,
// 127.0.0.0/8 or ::1) and returns it normalised.
func CheckURL(raw string) (string, error) {
	u, err := url.Parse(raw)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" || u.User != nil {
		return "", errors.New("url must be an http:// or https:// address, like http://localhost:$LAZYBOX_PORT")
	}
	host := strings.ToLower(u.Hostname())
	if ip := net.ParseIP(host); host != "localhost" && !strings.HasSuffix(host, ".localhost") && (ip == nil || !ip.IsLoopback()) {
		return "", errors.New("url must point at this machine (localhost, 127.0.0.1 or ::1)")
	}
	return u.String(), nil
}

// Expand replaces $NAME and ${NAME} for the given variables only; anything
// else, including other $ signs, is left for the shell.
func Expand(s string, vars map[string]string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		if s[i] != '$' {
			b.WriteByte(s[i])
			continue
		}
		name, width := "", 0
		if i+1 < len(s) && s[i+1] == '{' {
			if end := strings.IndexByte(s[i+2:], '}'); end >= 0 {
				name, width = s[i+2:i+2+end], end+3
			}
		} else {
			j := i + 1
			for j < len(s) && (s[j] == '_' || s[j] >= 'A' && s[j] <= 'Z' || s[j] >= 'a' && s[j] <= 'z' || s[j] >= '0' && s[j] <= '9') {
				j++
			}
			name, width = s[i+1:j], j-i
		}
		if v, ok := vars[name]; ok && name != "" {
			b.WriteString(v)
			i += width - 1
			continue
		}
		b.WriteByte('$')
	}
	return b.String()
}

// hashVersion changes when the hash's input changes meaning, so an approval
// never covers a configuration it was not given for.
const hashVersion = "lazybox-workspace-v1\n"

// Hash identifies exactly what a person approves: the parsed configuration,
// independent of comments and formatting, which run nothing.
func (c Config) Hash() string {
	data, _ := json.Marshal(c)
	sum := sha256.Sum256(append([]byte(hashVersion), data...))
	return hex.EncodeToString(sum[:])
}

// Entries are the configuration's reviewable parts, each as canonical JSON:
// "setup", "archive", "files", "env" and "action:<name>".
func (c Config) Entries() map[string]string {
	out := map[string]string{}
	put := func(key string, v any) {
		data, _ := json.Marshal(v)
		out[key] = string(data)
	}
	if c.Setup != nil {
		put("setup", c.Setup)
	}
	if c.Archive != nil {
		put("archive", c.Archive)
	}
	if len(c.Files) > 0 {
		put("files", c.Files)
	}
	if len(c.Env) > 0 {
		put("env", c.Env)
	}
	for _, a := range c.Actions {
		put("action:"+a.Name, a)
	}
	return out
}

// Changed lists the entries that differ between two Entries maps, added and
// removed ones included, sorted.
func Changed(before, after map[string]string) []string {
	out := []string{}
	for key, v := range after {
		if before[key] != v {
			out = append(out, key)
		}
	}
	for key := range before {
		if _, ok := after[key]; !ok {
			out = append(out, key)
		}
	}
	sort.Strings(out)
	return out
}

// Source says who wrote an effective entry.
const (
	FromRepository = "repository"
	FromSettings   = "settings"
)

// EffectiveAction is an action and who wrote it.
type EffectiveAction struct {
	Action
	Source string
}

// EffectiveScript is a setup or archive script and who wrote it.
type EffectiveScript struct {
	Script
	Source string
}

// Effective is what runs for one member: the repository's configuration
// (only once approved) with the person's own overrides on top.
type Effective struct {
	Setup   *EffectiveScript
	Archive *EffectiveScript
	Files   []string
	Env     map[string]string
	Actions []EffectiveAction
}

// ActionNamed finds an effective action by name.
func (e Effective) ActionNamed(name string) (Action, bool) {
	for _, a := range e.Actions {
		if a.Name == name {
			return a.Action, true
		}
	}
	return Action{}, false
}

// Merge applies overrides (written by the person in Settings) over the
// repository's configuration. Pass a zero repository Config when it is not
// approved: nothing from an unapproved file runs, its env included, since a
// variable like NODE_OPTIONS runs code too.
func Merge(repo, overrides Config) Effective {
	e := Effective{Env: map[string]string{}}
	if repo.Setup != nil {
		e.Setup = &EffectiveScript{*repo.Setup, FromRepository}
	}
	if overrides.Setup != nil {
		e.Setup = &EffectiveScript{*overrides.Setup, FromSettings}
	}
	if repo.Archive != nil {
		e.Archive = &EffectiveScript{*repo.Archive, FromRepository}
	}
	if overrides.Archive != nil {
		e.Archive = &EffectiveScript{*overrides.Archive, FromSettings}
	}
	seen := map[string]bool{}
	for _, f := range append(append([]string{}, repo.Files...), overrides.Files...) {
		if !seen[f] {
			seen[f] = true
			e.Files = append(e.Files, f)
		}
	}
	for k, v := range repo.Env {
		e.Env[k] = v
	}
	for k, v := range overrides.Env {
		e.Env[k] = v
	}
	for _, a := range repo.Actions {
		if o, ok := overrides.Action(a.Name); ok {
			e.Actions = append(e.Actions, EffectiveAction{o, FromSettings})
		} else {
			e.Actions = append(e.Actions, EffectiveAction{a, FromRepository})
		}
	}
	for _, o := range overrides.Actions {
		if _, ok := repo.Action(o.Name); !ok {
			e.Actions = append(e.Actions, EffectiveAction{o, FromSettings})
		}
	}
	return e
}
