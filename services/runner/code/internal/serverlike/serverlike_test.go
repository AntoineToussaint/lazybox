package serverlike

import (
	"encoding/json"
	"os"
	"testing"
)

// The vectors are shared with the client's mirror of these rules
// (scripts/code-run.test.ts reads the same file), so the two cannot drift.
func TestCommandsMatchTheSharedVectors(t *testing.T) {
	data, err := os.ReadFile("testdata/commands.json")
	if err != nil {
		t.Fatal(err)
	}
	var vectors []struct {
		Script string `json:"script"`
		Server bool   `json:"server"`
	}
	if err = json.Unmarshal(data, &vectors); err != nil {
		t.Fatal(err)
	}
	if len(vectors) < 40 {
		t.Fatalf("too few vectors: %d", len(vectors))
	}
	for _, v := range vectors {
		if got, why := Match(v.Script); got != v.Server {
			t.Errorf("Match(%q) = %v (%q), want %v", v.Script, got, why, v.Server)
		}
	}
}

func TestMatchArgvReadsTheShellScript(t *testing.T) {
	for _, c := range []struct {
		argv []string
		want bool
	}{
		{[]string{"/bin/sh", "-lc", "cd web && npm run dev"}, true},
		{[]string{"bash", "-c", "go test ./..."}, false},
		{[]string{"npm", "run", "dev"}, true},
		{[]string{"docker", "compose", "up", "-d"}, false},
	} {
		if got, _ := MatchArgv(c.argv); got != c.want {
			t.Errorf("MatchArgv(%q) = %v, want %v", c.argv, got, c.want)
		}
	}
	if _, why := Match("cd web && npm run dev"); why != "npm run dev" {
		t.Errorf("the reason names the matching command: %q", why)
	}
}
