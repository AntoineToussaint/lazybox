package runner

import (
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/githubconnection"
	"testing"
)

func TestGitHubAppIsRunnerConfigurationAndSurvivesRestart(t *testing.T) {
	cfg := Config{State: t.TempDir(), GitHubApp: &githubconnection.App{ClientID: "Iv.fixture", Slug: "fixture-app"}}
	bench, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	state := bench.GitHub.Status()
	if state.State != "not_connected" || state.App != *cfg.GitHubApp {
		t.Fatal(state)
	}
	if err = bench.Close(); err != nil {
		t.Fatal(err)
	}
	cfg.GitHubApp = nil
	bench, err = New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer bench.Close()
	if bench.GitHub.Status().App != state.App {
		t.Fatal("lost app registration on restart")
	}
}
