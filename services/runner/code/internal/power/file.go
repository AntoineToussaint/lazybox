package power

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"net/url"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"time"

	"gopkg.in/yaml.v3"
)

// Limits keep a mistaken file from flooding the menus or the machine.
const (
	MaxFileBytes   = 256 << 10
	MaxActions     = 100
	MaxHooks       = 50
	MaxSteps       = 20
	MaxStepTimeout = 24 * time.Hour
	// DefaultHookTimeout and MaxHookTimeout bound one hook run.
	DefaultHookTimeout = time.Minute
	MaxHookTimeout     = 10 * time.Minute
)

// Where an action is offered.
const (
	WherePalette      = "palette"
	WhereMenu         = "menu"
	WhereConversation = "conversation"
	WhereWorkspace    = "workspace"
)

var whereValues = []string{WherePalette, WhereMenu, WhereConversation, WhereWorkspace}

// Step kinds.
const (
	StepSend       = "send"
	StepOpen       = "open"
	StepRun        = "run"
	StepCommand    = "command"
	StepPermission = "permission"
	StepWait       = "wait"
	// StepDo runs an action of the app's registry (`lazybox do --list`).
	StepDo = "do"
)

var stepKinds = []string{StepSend, StepOpen, StepRun, StepCommand, StepPermission, StepWait, StepDo}

// Panels an open step may show: the app's panel kinds, plus settings and the inbox.
var Panels = []string{"chat", "console", "terminal", "assistant", "changes", "usage", "sessions", "missions", "pulls", "processes", "actions", "preview", "files", "editor", "artifacts", "settings", "inbox"}

// Grants a hook may hold, as a paired phone's (REMOTE.md): read the inbox and
// conversations; respond (send, answer, approve, interrupt); start conversations.
var Grants = []string{"read", "respond", "start"}

// Action is one named sequence of steps the app offers.
type Action struct {
	ID, Title, Description, Icon string
	// Key follows the leader in the ]] menu.
	Key     string
	Where   []string
	Confirm bool
	Steps   []Step
	Line    int
}

// Step is one thing an action does. Kind selects the fields that apply.
type Step struct {
	Kind, Name string
	// When is always, succeeded or failed: whether the step runs, given the
	// result of the last step before it that has no condition (the app runs
	// actions; POWER.md).
	When string
	// send
	Text, Snippet, To string
	Wait              bool
	// open
	Panel, URL, File string
	FileLine         int
	// run, command, permission, wait
	Action, Command, Permission, Until string
	Timeout                            time.Duration
	// do: a registry action and its argument
	Do, DoArgument string
	Line           int
}

// Hook runs a program, a shell line or a webhook on events.
type Hook struct {
	ID, Description string
	On              []string
	// Filters; each set one must match.
	Workspaces, Conversations, Repositories []string
	Run                                     []string
	Shell, Webhook                          string
	Headers                                 map[string]string
	Grants                                  []string
	Timeout                                 time.Duration
	Enabled                                 bool
	Line                                    int
}

// Target says what the hook runs, for people.
func (h Hook) Target() string {
	switch {
	case h.Webhook != "":
		return "POST " + h.Webhook
	case h.Shell != "":
		return h.Shell
	}
	return strings.Join(h.Run, " ")
}

// Issue is one problem with the file, located.
type Issue struct {
	Path     string `json:"path"`
	Line     int    `json:"line,omitempty"`
	Message  string `json:"message"`
	Severity string `json:"severity"`
}

func (i Issue) String() string {
	where := i.Path
	if i.Line > 0 {
		where = fmt.Sprintf("line %d (%s)", i.Line, i.Path)
	}
	return fmt.Sprintf("%s: %s: %s", i.Severity, where, i.Message)
}

// File is what a file declares: only the entries that validated.
type File struct {
	Actions []Action
	Hooks   []Hook
}

// Parse reads an actions file. An error means the file as a whole cannot be
// used (not YAML, not a mapping, too large); otherwise every entry with a
// problem is left out and reported, and the rest load.
func Parse(data []byte) (File, []Issue, error) {
	if len(data) > MaxFileBytes {
		return File{}, nil, fmt.Errorf("the file is larger than %d KiB", MaxFileBytes>>10)
	}
	var doc yaml.Node
	decoder := yaml.NewDecoder(bytes.NewReader(data))
	if err := decoder.Decode(&doc); err != nil {
		if errors.Is(err, io.EOF) {
			return File{}, nil, nil
		}
		return File{}, nil, fmt.Errorf("not valid YAML: %s", strings.TrimPrefix(err.Error(), "yaml: "))
	}
	var extra yaml.Node
	if err := decoder.Decode(&extra); err == nil {
		return File{}, nil, errors.New("the file holds more than one YAML document; keep one")
	}
	p := &parser{}
	root := &doc
	if root.Kind == yaml.DocumentNode && len(root.Content) == 1 {
		root = root.Content[0]
	}
	if root.Kind == yaml.ScalarNode && root.Tag == "!!null" {
		return File{}, nil, nil
	}
	if root.Kind != yaml.MappingNode {
		return File{}, nil, errors.New("the file must be a mapping with actions: and hooks:")
	}
	out := File{}
	for key, value := range pairs(root) {
		if (key.Value == "actions" || key.Value == "hooks") && value.Tag == "!!null" {
			continue // a section with only comments in it
		}
		switch key.Value {
		case "version":
			if value.Value != "1" {
				p.fail(value, "version", "only version 1 exists")
			}
		case "actions":
			out.Actions = p.actions(value)
		case "hooks":
			out.Hooks = p.hooks(value)
		default:
			p.warn(key, key.Value, "unknown section %q is ignored (expected version, actions, hooks)", key.Value)
		}
	}
	return out, p.issues, nil
}

type parser struct {
	issues []Issue
	// failed marks the entry being parsed as left out.
	failed bool
}

func (p *parser) fail(n *yaml.Node, path, format string, args ...any) {
	p.failed = true
	p.issues = append(p.issues, Issue{Path: path, Line: n.Line, Message: fmt.Sprintf(format, args...), Severity: "error"})
}

func (p *parser) warn(n *yaml.Node, path, format string, args ...any) {
	p.issues = append(p.issues, Issue{Path: path, Line: n.Line, Message: fmt.Sprintf(format, args...), Severity: "warning"})
}

// pairs iterates a mapping's keys and values in order.
func pairs(n *yaml.Node) func(func(*yaml.Node, *yaml.Node) bool) {
	return func(yield func(*yaml.Node, *yaml.Node) bool) {
		for i := 0; i+1 < len(n.Content); i += 2 {
			if !yield(n.Content[i], n.Content[i+1]) {
				return
			}
		}
	}
}

var idPattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,47}$`)
var headerPattern = regexp.MustCompile(`^[A-Za-z0-9-]{1,64}$`)
var keyPattern = regexp.MustCompile(`^(?:[a-z0-9]|Shift\+[a-z])$`)

func (p *parser) str(n *yaml.Node, path string, limit int) string {
	if n.Kind != yaml.ScalarNode || n.Tag == "!!null" {
		p.fail(n, path, "must be text")
		return ""
	}
	if len(n.Value) > limit {
		p.fail(n, path, "is longer than %d characters", limit)
		return ""
	}
	return n.Value
}

func (p *parser) boolean(n *yaml.Node, path string) bool {
	if n.Kind != yaml.ScalarNode || n.Tag != "!!bool" {
		p.fail(n, path, "must be true or false")
		return false
	}
	return n.Value == "true"
}

// list accepts one value or a list of them.
func (p *parser) list(n *yaml.Node, path string, limit int) []string {
	if n.Kind == yaml.ScalarNode {
		if v := p.str(n, path, limit); v != "" {
			return []string{v}
		}
		return nil
	}
	if n.Kind != yaml.SequenceNode {
		p.fail(n, path, "must be a value or a list of values")
		return nil
	}
	var out []string
	for i, item := range n.Content {
		if v := p.str(item, fmt.Sprintf("%s[%d]", path, i), limit); v != "" {
			out = append(out, v)
		}
	}
	return out
}

func (p *parser) oneOf(n *yaml.Node, path, value string, allowed []string) bool {
	if slices.Contains(allowed, value) {
		return true
	}
	p.fail(n, path, "%q is not one of %s", value, strings.Join(allowed, ", "))
	return false
}

// duration accepts "90s", "10m", "1h30m" or a number of seconds.
func (p *parser) duration(n *yaml.Node, path string, limit time.Duration) time.Duration {
	if n.Kind != yaml.ScalarNode {
		p.fail(n, path, "must be a duration such as 90s or 10m")
		return 0
	}
	var d time.Duration
	if seconds, err := strconv.Atoi(n.Value); err == nil {
		d = time.Duration(seconds) * time.Second
	} else if parsed, err := time.ParseDuration(n.Value); err == nil {
		d = parsed
	} else {
		p.fail(n, path, "%q is not a duration such as 90s or 10m", n.Value)
		return 0
	}
	if d <= 0 || d > limit {
		p.fail(n, path, "must be between 1s and %s", limit)
		return 0
	}
	return d
}

func (p *parser) actions(n *yaml.Node) []Action {
	if n.Kind != yaml.SequenceNode {
		p.fail(n, "actions", "must be a list of actions")
		return nil
	}
	var out []Action
	ids, keys := map[string]bool{}, map[string]string{}
	for i, item := range n.Content {
		path := fmt.Sprintf("actions[%d]", i)
		if i >= MaxActions {
			p.fail(item, path, "the file holds at most %d actions", MaxActions)
			break
		}
		p.failed = false
		a := p.action(item, path)
		if p.failed {
			continue
		}
		if ids[a.ID] {
			p.fail(item, path+".id", "another action already has the id %q", a.ID)
			continue
		}
		if a.Key != "" {
			if other, taken := keys[a.Key]; taken {
				p.fail(item, path+".key", "key %q is already used by %q", a.Key, other)
				continue
			}
			keys[a.Key] = a.ID
		}
		ids[a.ID] = true
		out = append(out, a)
	}
	return out
}

func (p *parser) action(n *yaml.Node, path string) Action {
	a := Action{Line: n.Line}
	if n.Kind != yaml.MappingNode {
		p.fail(n, path, "an action is a mapping with id, title and steps")
		return a
	}
	var steps *yaml.Node
	for key, value := range pairs(n) {
		at := path + "." + key.Value
		switch key.Value {
		case "id":
			a.ID = p.str(value, at, 48)
			if a.ID != "" && !idPattern.MatchString(a.ID) {
				p.fail(value, at, "%q must be lowercase letters, digits and dashes", a.ID)
			}
		case "title":
			a.Title = p.str(value, at, 80)
		case "description":
			a.Description = p.str(value, at, 500)
		case "icon":
			a.Icon = p.str(value, at, 32)
		case "key":
			a.Key = p.str(value, at, 7)
			if a.Key != "" && !keyPattern.MatchString(a.Key) {
				p.fail(value, at, "%q must be one lowercase letter or digit (or Shift+letter); it follows the ]] leader", a.Key)
			}
		case "where":
			for i, w := range p.list(value, at, 20) {
				if p.oneOf(value, fmt.Sprintf("%s[%d]", at, i), w, whereValues) && !slices.Contains(a.Where, w) {
					a.Where = append(a.Where, w)
				}
			}
		case "confirm":
			a.Confirm = p.boolean(value, at)
		case "steps":
			steps = value
		default:
			p.fail(key, at, "unknown field %q (expected id, title, description, icon, key, where, confirm, steps)", key.Value)
		}
	}
	if a.ID == "" {
		p.fail(n, path+".id", "an action needs an id")
	}
	if a.Title == "" && a.ID != "" {
		a.Title = humanize(a.ID)
	}
	if len(a.Where) == 0 {
		a.Where = []string{WherePalette, WhereMenu}
	}
	if steps == nil {
		p.fail(n, path+".steps", "an action needs steps")
		return a
	}
	if steps.Kind != yaml.SequenceNode || len(steps.Content) == 0 {
		p.fail(steps, path+".steps", "steps must be a non-empty list")
		return a
	}
	if len(steps.Content) > MaxSteps {
		p.fail(steps, path+".steps", "an action has at most %d steps", MaxSteps)
		return a
	}
	for i, item := range steps.Content {
		a.Steps = append(a.Steps, p.step(item, fmt.Sprintf("%s.steps[%d]", path, i), i))
	}
	return a
}

func humanize(id string) string {
	s := strings.ReplaceAll(id, "-", " ")
	return strings.ToUpper(s[:1]) + s[1:]
}

func (p *parser) step(n *yaml.Node, path string, index int) Step {
	s := Step{Line: n.Line, When: "always"}
	if n.Kind != yaml.MappingNode {
		p.fail(n, path, "a step is a mapping such as {send: \"…\"} or {open: changes}")
		return s
	}
	for key, value := range pairs(n) {
		at := path + "." + key.Value
		if slices.Contains(stepKinds, key.Value) {
			if s.Kind != "" {
				p.fail(key, at, "a step does one thing: it already has %s", s.Kind)
				continue
			}
			s.Kind = key.Value
			p.stepBody(&s, value, at)
			continue
		}
		switch key.Value {
		case "name":
			s.Name = p.str(value, at, 80)
		case "when":
			s.When = p.str(value, at, 20)
			if s.When != "" && p.oneOf(value, at, s.When, []string{"always", "succeeded", "failed"}) && index == 0 && s.When != "always" {
				p.fail(value, at, "the first step has no previous step to depend on")
			}
		case "timeout":
			s.Timeout = p.duration(value, at, MaxStepTimeout)
		default:
			p.fail(key, at, "unknown field %q (a step has one of %s, and optionally name, when, timeout)", key.Value, strings.Join(stepKinds, ", "))
		}
	}
	if s.Kind == "" {
		p.fail(n, path, "a step needs one of %s", strings.Join(stepKinds, ", "))
	}
	return s
}

func (p *parser) stepBody(s *Step, n *yaml.Node, path string) {
	switch s.Kind {
	case StepSend:
		s.To = "current"
		if n.Kind == yaml.ScalarNode {
			s.Text = p.str(n, path, 32<<10)
		} else if n.Kind == yaml.MappingNode {
			for key, value := range pairs(n) {
				at := path + "." + key.Value
				switch key.Value {
				case "text":
					s.Text = p.str(value, at, 32<<10)
				case "snippet":
					s.Snippet = p.str(value, at, 120)
				case "to":
					s.To = p.str(value, at, 200)
				case "wait":
					s.Wait = p.boolean(value, at)
				default:
					p.fail(key, at, "unknown field %q (expected text, snippet, to, wait)", key.Value)
				}
			}
		} else {
			p.fail(n, path, "send takes the text, or {text|snippet, to, wait}")
		}
		if (s.Text == "") == (s.Snippet == "") {
			p.fail(n, path, "send needs either text or a snippet")
		}
	case StepOpen:
		if n.Kind == yaml.ScalarNode {
			value := p.str(n, path, 2048)
			switch {
			case strings.HasPrefix(value, "http://") || strings.HasPrefix(value, "https://"):
				s.URL = value
			case slices.Contains(Panels, value):
				s.Panel = value
			default:
				p.fail(n, path, "%q is neither a panel (%s) nor a web address; open a file with {file: path, line: n}", value, strings.Join(Panels, ", "))
			}
		} else if n.Kind == yaml.MappingNode {
			for key, value := range pairs(n) {
				at := path + "." + key.Value
				switch key.Value {
				case "panel":
					s.Panel = p.str(value, at, 40)
					if s.Panel != "" {
						p.oneOf(value, at, s.Panel, Panels)
					}
				case "url":
					s.URL = p.str(value, at, 2048)
				case "file":
					s.File = p.str(value, at, 4096)
				case "line":
					line, err := strconv.Atoi(value.Value)
					if err != nil || line < 1 {
						p.fail(value, at, "line must be a positive number")
					}
					s.FileLine = line
				default:
					p.fail(key, at, "unknown field %q (expected panel, url, file, line)", key.Value)
				}
			}
		} else {
			p.fail(n, path, "open takes a panel, a web address, or {file, line}")
		}
		set := 0
		for _, v := range []string{s.Panel, s.URL, s.File} {
			if v != "" {
				set++
			}
		}
		if set != 1 {
			p.fail(n, path, "open needs exactly one of panel, url or file")
		}
		if s.URL != "" {
			if u, err := url.Parse(s.URL); err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
				p.fail(n, path, "%q is not an http or https address", s.URL)
			}
		}
		if s.File != "" && (strings.HasPrefix(s.File, "/") || strings.Contains("/"+s.File+"/", "/../")) {
			p.fail(n, path, "file is a path inside the workspace, such as api/README.md")
		}
		if s.FileLine > 0 && s.File == "" {
			p.fail(n, path, "line goes with file")
		}
	case StepRun:
		s.Action = p.str(n, path, 120)
		if s.Action == "" {
			p.fail(n, path, "run takes a workspace action's key, as the Actions panel lists it")
		}
	case StepCommand:
		s.Command = p.str(n, path, 8<<10)
		if strings.TrimSpace(s.Command) == "" {
			p.fail(n, path, "command takes a shell command")
		}
	case StepPermission:
		s.Permission = p.str(n, path, 20)
		if s.Permission != "" {
			p.oneOf(n, path, s.Permission, []string{"ask", "read-only", "yolo"})
		}
	case StepWait:
		s.Until = "idle"
		if n.Kind == yaml.ScalarNode {
			s.Until = p.str(n, path, 20)
		} else if n.Kind == yaml.MappingNode {
			for key, value := range pairs(n) {
				at := path + "." + key.Value
				switch key.Value {
				case "until":
					s.Until = p.str(value, at, 20)
				case "timeout":
					s.Timeout = p.duration(value, at, MaxStepTimeout)
				default:
					p.fail(key, at, "unknown field %q (expected until, timeout)", key.Value)
				}
			}
		} else {
			p.fail(n, path, "wait takes idle or needs-input")
		}
		p.oneOf(n, path, s.Until, []string{"idle", "needs-input"})
	case StepDo:
		if n.Kind == yaml.ScalarNode {
			s.Do = p.str(n, path, 80)
		} else if n.Kind == yaml.MappingNode {
			for key, value := range pairs(n) {
				at := path + "." + key.Value
				switch key.Value {
				case "action":
					s.Do = p.str(value, at, 80)
				case "argument":
					s.DoArgument = p.str(value, at, 2048)
				default:
					p.fail(key, at, "unknown field %q (expected action, argument)", key.Value)
				}
			}
		} else {
			p.fail(n, path, "do takes an action of the app, such as layout.maximize, or {action, argument}")
		}
		if s.Do == "" {
			p.fail(n, path, "do needs an action of the app (lazybox do --list)")
		} else if a, ok := registry.Action(s.Do); !ok {
			p.fail(n, path, "the app has no action %q%s (lazybox do --list lists every action)", s.Do, didYouMean(s.Do, registry.ActionIDs()))
		} else if s.DoArgument != "" && a.Argument == "" {
			p.fail(n, path, "%s takes no argument", a.ID)
		}
	}
}

func (p *parser) hooks(n *yaml.Node) []Hook {
	if n.Kind != yaml.SequenceNode {
		p.fail(n, "hooks", "must be a list of hooks")
		return nil
	}
	var out []Hook
	ids := map[string]bool{}
	for i, item := range n.Content {
		path := fmt.Sprintf("hooks[%d]", i)
		if i >= MaxHooks {
			p.fail(item, path, "the file holds at most %d hooks", MaxHooks)
			break
		}
		p.failed = false
		h := p.hook(item, path)
		if p.failed {
			continue
		}
		if ids[h.ID] {
			p.fail(item, path+".id", "another hook already has the id %q", h.ID)
			continue
		}
		ids[h.ID] = true
		out = append(out, h)
	}
	return out
}

func (p *parser) hook(n *yaml.Node, path string) Hook {
	h := Hook{Line: n.Line, Enabled: true, Timeout: DefaultHookTimeout}
	if n.Kind != yaml.MappingNode {
		p.fail(n, path, "a hook is a mapping with id, on, and run, shell or webhook")
		return h
	}
	for key, value := range pairs(n) {
		at := path + "." + key.Value
		switch key.Value {
		case "id":
			h.ID = p.str(value, at, 48)
			if h.ID != "" && !idPattern.MatchString(h.ID) {
				p.fail(value, at, "%q must be lowercase letters, digits and dashes", h.ID)
			}
		case "description":
			h.Description = p.str(value, at, 500)
		case "on":
			for i, e := range p.list(value, at, 40) {
				if p.oneOf(value, fmt.Sprintf("%s[%d]", at, i), e, Events) && !slices.Contains(h.On, e) {
					h.On = append(h.On, e)
				}
			}
		case "filter":
			if value.Kind != yaml.MappingNode {
				p.fail(value, at, "filter is a mapping of workspace, conversation, repository")
				continue
			}
			for fk, fv := range pairs(value) {
				fat := at + "." + fk.Value
				switch fk.Value {
				case "workspace":
					h.Workspaces = p.list(fv, fat, 200)
				case "conversation":
					h.Conversations = p.list(fv, fat, 200)
				case "repository":
					h.Repositories = p.list(fv, fat, 200)
				default:
					p.fail(fk, fat, "unknown filter %q (expected workspace, conversation, repository)", fk.Value)
				}
			}
		case "run":
			h.Run = p.list(value, at, 4096)
			if len(h.Run) > 64 {
				p.fail(value, at, "at most 64 arguments")
			}
		case "shell":
			h.Shell = p.str(value, at, 8<<10)
		case "webhook":
			if value.Kind == yaml.MappingNode {
				for wk, wv := range pairs(value) {
					wat := at + "." + wk.Value
					switch wk.Value {
					case "url":
						h.Webhook = p.str(wv, wat, 2048)
					case "headers":
						h.Headers = p.headers(wv, wat)
					default:
						p.fail(wk, wat, "unknown field %q (expected url, headers)", wk.Value)
					}
				}
			} else {
				h.Webhook = p.str(value, at, 2048)
			}
			if h.Webhook != "" {
				if u, err := url.Parse(h.Webhook); err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
					p.fail(value, at, "%q is not an http or https address", h.Webhook)
				}
			}
		case "grants":
			for i, g := range p.list(value, at, 20) {
				if p.oneOf(value, fmt.Sprintf("%s[%d]", at, i), g, Grants) && !slices.Contains(h.Grants, g) {
					h.Grants = append(h.Grants, g)
				}
			}
		case "timeout":
			if d := p.duration(value, at, MaxHookTimeout); d > 0 {
				h.Timeout = d
			}
		case "enabled":
			h.Enabled = p.boolean(value, at)
		default:
			p.fail(key, at, "unknown field %q (expected id, description, on, filter, run, shell, webhook, grants, timeout, enabled)", key.Value)
		}
	}
	if h.ID == "" {
		p.fail(n, path+".id", "a hook needs an id")
	}
	if len(h.On) == 0 {
		p.fail(n, path+".on", "a hook needs on: one or more of %s", strings.Join(Events, ", "))
	}
	targets := 0
	for _, set := range []bool{len(h.Run) > 0, h.Shell != "", h.Webhook != ""} {
		if set {
			targets++
		}
	}
	if targets != 1 {
		p.fail(n, path, "a hook needs exactly one of run, shell or webhook")
	}
	if h.Webhook != "" && len(h.Grants) > 0 {
		p.fail(n, path+".grants", "a webhook runs no lazybox command; grants are for run and shell hooks")
	}
	return h
}

func (p *parser) headers(n *yaml.Node, path string) map[string]string {
	if n.Kind != yaml.MappingNode {
		p.fail(n, path, "headers is a mapping of names to values")
		return nil
	}
	out := map[string]string{}
	for k, v := range pairs(n) {
		name := k.Value
		if !headerPattern.MatchString(name) {
			p.fail(k, path, "%q is not a header name", name)
			continue
		}
		out[name] = p.str(v, path+"."+name, 4096)
	}
	return out
}
