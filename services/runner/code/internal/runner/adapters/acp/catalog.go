package acp

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"
	"unicode"
)

// Agent is one ACP agent program the Runner can run: how to start it in ACP
// mode, and how people know it.
type Agent struct {
	// ID is stable and short: "gemini", or a custom agent's slug.
	ID    string `json:"id"`
	Title string `json:"title"`
	// Command is a program name found on the Runner's PATH, or an absolute path.
	Command string   `json:"command"`
	Args    []string `json:"args,omitempty"`
	// Env is added to the agent's environment. Values are private: they are
	// kept in the Runner's owner-only state and never returned on the wire.
	Env map[string]string `json:"env,omitempty"`
	// Icon names the client's icon for the agent; "agent" when it has none.
	Icon string `json:"icon,omitempty"`
	// Install is a command that installs a built-in agent that is missing.
	Install  string `json:"install,omitempty"`
	Homepage string `json:"homepage,omitempty"`
	// Custom marks an agent the person added (or a built-in they changed).
	Custom bool `json:"-"`
}

// Builtins are the ACP agents Lazybox knows how to start, each detected on the
// Runner's PATH. Claude Code and Codex are not here: they keep their native
// adapters, which do more than their ACP bridges (ACP.md). The order is the
// order every surface lists them.
var Builtins = []Agent{
	{ID: "gemini", Title: "Gemini CLI", Command: "gemini", Args: []string{"--acp"}, Icon: "agent-gemini", Install: "npm install -g @google/gemini-cli", Homepage: "https://github.com/google-gemini/gemini-cli"},
	{ID: "goose", Title: "Goose", Command: "goose", Args: []string{"acp"}, Icon: "agent-goose", Install: "brew install block-goose-cli", Homepage: "https://goose-docs.ai"},
	{ID: "opencode", Title: "OpenCode", Command: "opencode", Args: []string{"acp"}, Icon: "agent-opencode", Install: "npm install -g opencode-ai", Homepage: "https://opencode.ai"},
	{ID: "qwen", Title: "Qwen Code", Command: "qwen", Args: []string{"--acp"}, Icon: "agent-qwen", Install: "npm install -g @qwen-code/qwen-code", Homepage: "https://github.com/QwenLM/qwen-code"},
	{ID: "copilot", Title: "GitHub Copilot", Command: "copilot", Args: []string{"--acp"}, Icon: "agent-copilot", Install: "npm install -g @github/copilot", Homepage: "https://github.com/features/copilot/cli"},
	{ID: "cursor", Title: "Cursor Agent", Command: "cursor-agent", Args: []string{"acp"}, Icon: "agent-cursor", Install: "curl https://cursor.com/install -fsS | bash", Homepage: "https://cursor.com/cli"},
	{ID: "kiro", Title: "Kiro CLI", Command: "kiro-cli", Args: []string{"acp"}, Icon: "agent-kiro", Install: "curl -fsSL https://cli.kiro.dev/install | bash", Homepage: "https://kiro.dev/cli"},
	{ID: "cline", Title: "Cline", Command: "cline", Args: []string{"--acp"}, Icon: "agent-cline", Install: "npm install -g cline", Homepage: "https://cline.bot"},
	{ID: "auggie", Title: "Auggie", Command: "auggie", Args: []string{"--acp"}, Icon: "agent-auggie", Install: "npm install -g @augmentcode/auggie", Homepage: "https://docs.augmentcode.com/cli/acp"},
}

// ProviderPrefix marks an ACP agent's provider name: "acp:gemini".
const ProviderPrefix = "acp:"

// Provider is the provider name of agent id; ProfileID its one profile, which
// runs on the agent's own sign-in.
func Provider(id string) string  { return ProviderPrefix + id }
func ProfileID(id string) string { return "acp-" + id }

// IsProvider reports whether provider names an ACP agent.
func IsProvider(provider string) bool {
	return strings.HasPrefix(provider, ProviderPrefix) && ValidID(strings.TrimPrefix(provider, ProviderPrefix))
}

// AgentID is the agent a provider name names, or "".
func AgentID(provider string) string {
	if !IsProvider(provider) {
		return ""
	}
	return strings.TrimPrefix(provider, ProviderPrefix)
}

var (
	idPattern  = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,31}$`)
	envPattern = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]{0,127}$`)
)

// ValidID reports whether id can name an agent.
func ValidID(id string) bool { return idPattern.MatchString(id) }

// Slug is a valid id made from a title: "My Agent!" → "my-agent".
func Slug(title string) string {
	var b strings.Builder
	dash := false
	for _, r := range strings.ToLower(title) {
		switch {
		case r >= 'a' && r <= 'z' || r >= '0' && r <= '9':
			b.WriteRune(r)
			dash = false
		case b.Len() > 0 && !dash:
			b.WriteByte('-')
			dash = true
		}
	}
	id := strings.Trim(b.String(), "-")
	if len(id) > 32 {
		id = strings.Trim(id[:32], "-")
	}
	return id
}

// Validate checks a custom agent the person entered.
func (a Agent) Validate() error {
	switch {
	case !ValidID(a.ID):
		return errors.New("the agent's id is 1 to 32 lowercase letters, digits or dashes")
	case strings.TrimSpace(a.Title) == "" || len(a.Title) > 64 || strings.IndexFunc(a.Title, unicode.IsControl) >= 0:
		return errors.New("the agent needs a name of at most 64 characters")
	case strings.TrimSpace(a.Command) == "" || len(a.Command) > 1024 || strings.ContainsAny(a.Command, "\x00\r\n"):
		return errors.New("the agent needs the command that starts it")
	case strings.ContainsRune(a.Command, filepath.Separator) && !filepath.IsAbs(a.Command):
		return errors.New("the command is a program name on the PATH or an absolute path")
	case len(a.Args) > 64:
		return errors.New("at most 64 arguments")
	case len(a.Env) > 32:
		return errors.New("at most 32 environment variables")
	}
	for _, arg := range a.Args {
		if len(arg) > 4096 || strings.ContainsRune(arg, 0) {
			return errors.New("an argument is too long or holds a NUL byte")
		}
	}
	for name, value := range a.Env {
		if !envPattern.MatchString(name) {
			return fmt.Errorf("%q is not an environment variable name", name)
		}
		if len(value) > 16<<10 || strings.ContainsRune(value, 0) {
			return fmt.Errorf("the value of %s is too long or holds a NUL byte", name)
		}
	}
	return nil
}

// Registry is the agents a Runner can run: the built-ins and the person's own,
// kept in an owner-only file. A custom agent with a built-in's id replaces
// that built-in's command, arguments and environment.
type Registry struct {
	path   string
	mu     sync.Mutex
	custom []Agent
}

type registryFile struct {
	Agents []Agent `json:"agents"`
}

// OpenRegistry loads the person's agents from path (absent is none).
func OpenRegistry(path string) (*Registry, error) {
	if !filepath.IsAbs(path) {
		return nil, errors.New("absolute ACP agent registry path required")
	}
	r := &Registry{path: path}
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return r, nil
	}
	if err != nil {
		return nil, err
	}
	var file registryFile
	if err = json.Unmarshal(data, &file); err != nil {
		return nil, fmt.Errorf("ACP agent registry %s: %w", path, err)
	}
	for _, a := range file.Agents {
		if a.Validate() == nil {
			a.Custom = true
			r.custom = append(r.custom, a)
		}
	}
	return r, nil
}

// Agents is every agent, built-ins first in their order, then the person's
// own by title. Env maps are copies.
func (r *Registry) Agents() []Agent {
	r.mu.Lock()
	defer r.mu.Unlock()
	byID := map[string]Agent{}
	for _, a := range r.custom {
		byID[a.ID] = a
	}
	out := []Agent{}
	for _, b := range Builtins {
		if c, ok := byID[b.ID]; ok {
			b.Title, b.Command, b.Args, b.Env, b.Custom = c.Title, c.Command, c.Args, c.Env, true
			delete(byID, b.ID)
		}
		out = append(out, copyAgent(b))
	}
	rest := []Agent{}
	for _, a := range byID {
		if a.Icon == "" {
			a.Icon = "agent"
		}
		rest = append(rest, copyAgent(a))
	}
	sort.Slice(rest, func(i, j int) bool {
		return strings.ToLower(rest[i].Title) < strings.ToLower(rest[j].Title) || (strings.EqualFold(rest[i].Title, rest[j].Title) && rest[i].ID < rest[j].ID)
	})
	return append(out, rest...)
}

// Get returns agent id.
func (r *Registry) Get(id string) (Agent, bool) {
	for _, a := range r.Agents() {
		if a.ID == id {
			return a, true
		}
	}
	return Agent{}, false
}

// Save adds or replaces the person's agent. An env value left empty keeps the
// one saved before under that name, so a client that never sees the values
// can still edit the rest.
func (r *Registry) Save(a Agent) (Agent, error) {
	a.Title = strings.TrimSpace(a.Title)
	a.Command = strings.TrimSpace(a.Command)
	if a.ID == "" {
		a.ID = Slug(a.Title)
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	next := []Agent{}
	for _, old := range r.custom {
		if old.ID == a.ID {
			for name, value := range a.Env {
				if value == "" {
					a.Env[name] = old.Env[name]
				}
			}
			continue
		}
		next = append(next, old)
	}
	for name, value := range a.Env {
		if value == "" {
			delete(a.Env, name)
		}
	}
	if err := a.Validate(); err != nil {
		return Agent{}, err
	}
	a.Custom = true
	a.Install, a.Homepage = "", ""
	if !isBuiltin(a.ID) {
		a.Icon = "agent"
	} else {
		a.Icon = ""
	}
	next = append(next, a)
	if err := r.write(next); err != nil {
		return Agent{}, err
	}
	r.custom = next
	return a, nil
}

// Remove deletes the person's agent id; a changed built-in goes back to its
// default command.
func (r *Registry) Remove(id string) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	next := []Agent{}
	found := false
	for _, a := range r.custom {
		if a.ID == id {
			found = true
			continue
		}
		next = append(next, a)
	}
	if !found {
		return os.ErrNotExist
	}
	if err := r.write(next); err != nil {
		return err
	}
	r.custom = next
	return nil
}

func (r *Registry) write(agents []Agent) error {
	data, err := json.MarshalIndent(registryFile{Agents: agents}, "", "  ")
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(r.path), 0700); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(r.path), ".acp-agents-*")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	if err = tmp.Chmod(0600); err == nil {
		_, err = tmp.Write(append(data, '\n'))
	}
	if closeErr := tmp.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(tmp.Name(), r.path)
}

func isBuiltin(id string) bool {
	for _, b := range Builtins {
		if b.ID == id {
			return true
		}
	}
	return false
}

func copyAgent(a Agent) Agent {
	a.Args = append([]string(nil), a.Args...)
	if a.Env != nil {
		env := make(map[string]string, len(a.Env))
		for k, v := range a.Env {
			env[k] = v
		}
		a.Env = env
	}
	return a
}

// Program finds the agent's executable with lookPath (exec.LookPath): an
// absolute command as it is, a name on the PATH. Empty when it is missing.
func (a Agent) Program(lookPath func(string) (string, error)) string {
	if filepath.IsAbs(a.Command) {
		if info, err := os.Stat(a.Command); err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0 {
			return a.Command
		}
		return ""
	}
	if lookPath == nil {
		return ""
	}
	found, err := lookPath(a.Command)
	if err != nil {
		return ""
	}
	return found
}

// EnvNames are the names of the agent's private environment, sorted.
func (a Agent) EnvNames() []string {
	names := make([]string, 0, len(a.Env))
	for name := range a.Env {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}
