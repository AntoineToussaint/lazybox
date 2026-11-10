package gitidentity

import "testing"

func TestIdentityDependsOnCommonDirectory(t *testing.T) {
	if ID("/repos/api/.git") != ID("/repos/api/.git") || ID("/repos/api/.git") == ID("/repos/other-clone/.git") || len(ID("/repos/api/.git")) != 70 {
		t.Fatal("unstable or colliding identity")
	}
}

func TestGitHubIdentityIgnoresCloneLocationAndTransport(t *testing.T) {
	for _, remote := range []string{"https://github.com/Team/API.git", "git@github.com:team/api.git", "ssh://git@github.com/team/api", "ssh://git@ssh.github.com:443/team/api.git", "https://secret@github.com/team/api.git/"} {
		if GitHubName(remote) != "team/api" || Repository("/any/clone", remote) != "github:team/api" {
			t.Fatalf("unrecognized GitHub remote %q", remote)
		}
	}
	for _, remote := range []string{"", "/local/api", "git@work-alias:team/api.git", "https://github.com.evil.invalid/team/api", "https://github.com/team/api/extra", "https://github.com/team/api?token=secret", "https://github.com/team/%61pi", "file://github.com/team/api", "https://github.com:999/team/api"} {
		if GitHubName(remote) != "" || Repository("/local/.git", remote) != ID("/local/.git") {
			t.Fatalf("accepted ambiguous remote %q", remote)
		}
	}
	if Repository("/same", "https://github.com/fork/api") == Repository("/same", "https://github.com/team/api") {
		t.Fatal("fork conflated with upstream")
	}
}
