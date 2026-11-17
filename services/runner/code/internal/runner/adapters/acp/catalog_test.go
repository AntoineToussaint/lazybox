package acp

import (
	"os"
	"path/filepath"
	"testing"
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
