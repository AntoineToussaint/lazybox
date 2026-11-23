package acp

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestRegistryKeepsThePersonsAgentsPrivately(t *testing.T) {
	path := filepath.Join(t.TempDir(), "acp-agents.json")
	r, err := OpenRegistry(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(r.Agents()) != len(Builtins) {
		t.Fatal("a new registry lists only the built-in agents")
	}
	saved, err := r.Save(Agent{Title: "Team Agent!", Command: "npx", Args: []string{"@team/agent"}, Env: map[string]string{"TOKEN": "s3cret"}})
	if err != nil || saved.ID != "team-agent" || saved.Icon != "agent" || !saved.Custom {
		t.Fatalf("saved %+v, %v", saved, err)
	}
	if info, err := os.Stat(path); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("the registry file must be owner-only: %v %v", info.Mode(), err)
	}
	// An empty value keeps the stored one; a name left out drops it.
	if _, err = r.Save(Agent{ID: "team-agent", Title: "Team agent", Command: "npx", Env: map[string]string{"TOKEN": "", "OTHER": "x"}}); err != nil {
		t.Fatal(err)
	}
	again, err := OpenRegistry(path)
	if err != nil {
		t.Fatal(err)
	}
	a, ok := again.Get("team-agent")
	if !ok || a.Env["TOKEN"] != "s3cret" || a.Env["OTHER"] != "x" || a.Title != "Team agent" || len(a.EnvNames()) != 2 {
		t.Fatalf("reopened: %+v", a)
	}
	if _, err = again.Save(Agent{ID: "team-agent", Title: "Team agent", Command: "npx", Env: map[string]string{"OTHER": ""}}); err != nil {
		t.Fatal(err)
	}
	if a, _ = again.Get("team-agent"); a.Env["TOKEN"] != "" || a.Env["OTHER"] != "x" {
		t.Fatalf("dropped name kept: %+v", a.Env)
	}
	for _, bad := range []Agent{
		{Title: "", Command: "x"},
		{Title: "Relative", Command: "bin/agent"},
		{Title: "Env", Command: "x", Env: map[string]string{"NOT VALID": "1"}},
		{ID: "Bad_ID", Title: "x", Command: "x"},
	} {
		if _, err = again.Save(bad); err == nil {
			t.Errorf("accepted %+v", bad)
		}
	}
	if err = again.Remove("team-agent"); err != nil {
		t.Fatal(err)
	}
	if err = again.Remove("gemini"); !os.IsNotExist(err) {
		t.Fatalf("removing an unchanged built-in: %v", err)
	}
	if !IsProvider("acp:team-agent") || IsProvider("acp:") || IsProvider("claude") || AgentID("acp:gemini") != "gemini" || Slug("  Hello, World  ") != "hello-world" {
		t.Fatal("provider names")
	}
}

func TestAntigravityAndHermesLaunchCommands(t *testing.T) {
	r, err := OpenRegistry(filepath.Join(t.TempDir(), "agents.json"))
	if err != nil {
		t.Fatal(err)
	}
	for id, command := range map[string]string{"antigravity": "agy", "hermes": "hermes"} {
		a, ok := r.Get(id)
		if !ok || a.Command != command || len(a.Args) != 1 || a.Args[0] != "acp" {
			t.Errorf("wrong launch command for %s: %s %v", id, a.Command, a.Args)
		}
	}
}

// Antigravity uses Gemini CLI's API key when it has none of its own; its own
// wins, and only the shared name crosses.
func TestAntigravitySharesGeminisAPIKey(t *testing.T) {
	r, err := OpenRegistry(filepath.Join(t.TempDir(), "agents.json"))
	if err != nil {
		t.Fatal(err)
	}
	if a, _ := r.Get("antigravity"); a.SharesCredentialsWith != "gemini" || len(a.Inherited) != 0 || len(a.LaunchEnv()) != 0 {
		t.Fatalf("nothing to share yet: %+v", a)
	}
	if _, err = r.Save(Agent{ID: "gemini", Title: "Gemini CLI", Command: "gemini", Args: []string{"--acp"}, Env: map[string]string{"GEMINI_API_KEY": "gemini-key", "GOOGLE_API_KEY": "google-key", "GEMINI_MODEL": "pro"}}); err != nil {
		t.Fatal(err)
	}
	a, _ := r.Get("antigravity")
	if got := a.LaunchEnv(); len(got) != 1 || got[0] != "GEMINI_API_KEY=gemini-key" {
		t.Fatalf("inherited launch env: %v", got)
	}
	if names := a.InheritedEnvNames(); len(names) != 1 || names[0] != "GEMINI_API_KEY" || len(a.EnvNames()) != 0 {
		t.Fatalf("names: inherited %v own %v", names, a.EnvNames())
	}
	// A changed Antigravity still shares; its own key wins.
	if _, err = r.Save(Agent{ID: "antigravity", Title: "Antigravity", Command: "agy", Args: []string{"acp"}, Env: map[string]string{"GEMINI_API_KEY": "own-key", "EXTRA": "x"}}); err != nil {
		t.Fatal(err)
	}
	a, _ = r.Get("antigravity")
	if got := a.LaunchEnv(); len(got) != 2 || got[0] != "EXTRA=x" || got[1] != "GEMINI_API_KEY=own-key" || len(a.InheritedEnvNames()) != 0 {
		t.Fatalf("own key must win: %v inherited %v", got, a.InheritedEnvNames())
	}
	// Removing its own key goes back to Gemini's.
	if _, err = r.Save(Agent{ID: "antigravity", Title: "Antigravity", Command: "agy", Args: []string{"acp"}, Env: map[string]string{"EXTRA": ""}}); err != nil {
		t.Fatal(err)
	}
	if a, _ = r.Get("antigravity"); len(a.LaunchEnv()) != 2 || a.LaunchEnv()[0] != "GEMINI_API_KEY=gemini-key" {
		t.Fatalf("back to Gemini's key: %v", a.LaunchEnv())
	}
	// An empty value of its own (a hand-edited registry; Save drops them) is
	// no key: the report and the launch both use Gemini's.
	blank := a
	blank.Env = map[string]string{"GEMINI_API_KEY": ""}
	blank.Inherited = inherited(blank, r.Agents())
	if got := blank.LaunchEnv(); len(got) != 1 || got[0] != "GEMINI_API_KEY=gemini-key" || len(blank.InheritedEnvNames()) != 1 {
		t.Fatalf("an empty own value: launch %v, report %v", got, blank.InheritedEnvNames())
	}
	// Nothing is shared once Gemini has no key; other agents share nothing.
	if err = r.Remove("gemini"); err != nil {
		t.Fatal(err)
	}
	if a, _ = r.Get("antigravity"); len(a.InheritedEnvNames()) != 0 {
		t.Fatalf("Gemini has no key: %v", a.InheritedEnvNames())
	}
	for _, a := range r.Agents() {
		if a.ID != "antigravity" && (a.SharesCredentialsWith != "" || len(a.Inherited) != 0) {
			t.Errorf("%s shares credentials", a.ID)
		}
	}
	// The registry file never records what is shared.
	data, err := os.ReadFile(r.path)
	if err != nil || strings.Contains(string(data), "gemini-key") || strings.Contains(string(data), "Inherited") {
		t.Fatalf("registry: %s %v", data, err)
	}
}

// The shared key reaches the agent's process, after the Runner's
// environment, and the agent's own value wins over it.
func TestSharedKeyReachesTheAgent(t *testing.T) {
	for name, tc := range map[string]struct {
		own  map[string]string
		want string
	}{
		"inherited": {nil, "gemini-key"},
		"own wins":  {map[string]string{"GEMINI_API_KEY": "own-key"}, "own-key"},
	} {
		t.Run(name, func(t *testing.T) {
			log := filepath.Join(t.TempDir(), "acp.log")
			t.Setenv("ACP_FAKE_LOG", log)
			t.Setenv("ACP_FAKE_LOG_ENV", "GEMINI_API_KEY")
			a := fakeAgent(t)
			a.ID, a.SharesCredentialsWith, a.SharedEnv = "antigravity", "gemini", []string{"GEMINI_API_KEY"}
			a.Env = tc.own
			a.Inherited = inherited(a, []Agent{a, {ID: "gemini", Env: map[string]string{"GEMINI_API_KEY": "gemini-key"}}})
			s, err := Adapter{Agent: a}.Open(context.Background(), agent.Open{Cwd: workspace(t), Env: append(os.Environ(), "GEMINI_API_KEY=runner-key")})
			if err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { _ = s.Close() })
			say(t, s, "hello")
			data, _ := os.ReadFile(log)
			if !strings.Contains(string(data), "env GEMINI_API_KEY="+tc.want+"\n") {
				t.Fatalf("agent saw:\n%s", data)
			}
		})
	}
}
