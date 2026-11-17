package main

import (
	"os"
	"os/exec"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Doctor says which lazybox agents reach: fine, fine though another one comes
// first in the person's startup files, or broken with the fix.
func TestDoctorReportsWhichLazyboxAgentsReach(t *testing.T) {
	cli := "/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox"
	for _, c := range []struct {
		name   string
		in     *pb.AgentCliCheck
		status string
		detail []string
		fix    string
	}{
		{"unchecked", nil, checkWarn, []string{"not checked"}, ""},
		{"clean", &pb.AgentCliCheck{Cli: cli, Fixed: true}, checkOK, []string{cli}, ""},
		{"shadowed and fixed", &pb.AgentCliCheck{Cli: cli, Shadow: "/Users/me/.cargo/bin/lazybox", ShadowIn: []string{"zsh -c", "zsh -lc"}, Fixed: true}, checkOK, []string{cli, "/Users/me/.cargo/bin/lazybox", "zsh -lc", "puts its own back first"}, ""},
		{"unfixed", &pb.AgentCliCheck{Cli: cli, Shell: "/bin/sh", Shadow: "/usr/local/bin/lazybox", Unfixed: []string{"sh -lc finds /usr/local/bin/lazybox"}}, checkFail, []string{"sh -lc finds /usr/local/bin/lazybox"}, "Remove or rename /usr/local/bin/lazybox"},
		{"probe failed", &pb.AgentCliCheck{Cli: cli, Shell: "/bin/zsh", Problem: "timed out"}, checkWarn, []string{"/bin/zsh", "timed out"}, ""},
	} {
		got := agentCLICheck(c.in)
		if got.ID != "agent-cli" || got.Status != c.status {
			t.Errorf("%s: %+v, want %s", c.name, got, c.status)
		}
		for _, want := range c.detail {
			if !strings.Contains(got.Detail, want) {
				t.Errorf("%s: detail %q lacks %q", c.name, got.Detail, want)
			}
		}
		if !strings.HasPrefix(got.Fix, c.fix) || (c.fix != "" && !strings.Contains(got.Fix, cli)) {
			t.Errorf("%s: fix %q", c.name, got.Fix)
		}
	}
}

// `lazybox --version` names Lazybox V2, so an agent can tell this CLI from
// V1's (which prints `lazybox 0.1.x`).
func TestVersionNamesLazyboxV2(t *testing.T) {
	if os.Getenv("LAZYBOX_TEST_VERSION_MAIN") == "1" {
		os.Args = []string{"lazybox", "--version"}
		main()
		return
	}
	cmd := exec.Command(os.Args[0], "-test.run=^TestVersionNamesLazyboxV2$")
	cmd.Env = append(os.Environ(), "LAZYBOX_TEST_VERSION_MAIN=1")
	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("%v: %s", err, out)
	}
	if line := strings.SplitN(string(out), "\n", 2)[0]; line != "lazybox "+version.String()+" (Lazybox V2)" {
		t.Fatalf("--version printed %q", line)
	}
}
