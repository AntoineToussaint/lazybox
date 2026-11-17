package accounts

import (
	"fmt"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestProfileResolutionDoesNotMutateSiblingOrParent(t *testing.T) {
	t.Setenv("BENCH_ACCOUNT_A", "a-value")
	t.Setenv("BENCH_ACCOUNT_B", "b-value")
	inherited := []string{"PATH=/bin", "HOME=/original", "CLAUDECODE=nested"}
	a, err := Resolve(Profile{ID: "a", Provider: "codex", Home: "/profiles/a", EnvRefs: map[string]string{"OPENAI_API_KEY": "BENCH_ACCOUNT_A"}}, inherited)
	if err != nil {
		t.Fatal(err)
	}
	b, err := Resolve(Profile{ID: "b", Provider: "codex", Home: "/profiles/b", EnvRefs: map[string]string{"OPENAI_API_KEY": "BENCH_ACCOUNT_B"}}, inherited)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(strings.Join(a, "\n"), "OPENAI_API_KEY=a-value") || !strings.Contains(strings.Join(b, "\n"), "OPENAI_API_KEY=b-value") {
		t.Fatal("profile credentials crossed")
	}
	if strings.Contains(strings.Join(a, "\n"), "CLAUDECODE=") || inherited[2] != "CLAUDECODE=nested" {
		t.Fatal("parent environment mutated or nested state inherited")
	}
	if _, err = Resolve(Profile{ID: "missing", Provider: "claude", EnvRefs: map[string]string{"ANTHROPIC_API_KEY": "BENCH_MISSING_CREDENTIAL"}}, nil); err == nil {
		t.Fatal("missing credential silently inherited")
	}
}

func TestSelectionHonorsEligibilityProviderAndCapacity(t *testing.T) {
	profiles := []Profile{{ID: "a", Provider: "codex", Isolation: "environment"}, {ID: "b", Provider: "codex", Isolation: "environment"}, {ID: "c", Provider: "claude", Isolation: "environment"}, {ID: "shared", Provider: "codex", Isolation: "inherited"}}
	selected, err := Select(profiles, []string{"shared", "c", "a", "b"}, map[string]Availability{"a": Exhausted}, "codex", "a")
	if err != nil || selected.ID != "b" {
		t.Fatalf("%+v %v", selected, err)
	}
	for _, reason := range []Availability{Exhausted, Throttled, AuthFailed, Unknown} {
		if _, err = Select(profiles, []string{"b"}, map[string]Availability{"b": reason}, "codex", "a"); err == nil {
			t.Fatalf("selected unavailable profile %s", reason)
		}
	}
	if _, err = Select(profiles, nil, nil, "codex", "a"); err == nil {
		t.Fatal("ignored explicit eligibility")
	}
}

func TestGatewayValidationRejectsEmbeddedCredentials(t *testing.T) {
	for _, url := range []string{"file:///etc/passwd", "https://user:secret@gateway.test", "https://gateway.test?token=secret", "https://gateway.test/#secret"} {
		if err := (Options{Gateway: &agent.Gateway{ID: "meter", URL: url}}).Validate(); err == nil {
			t.Fatal(url)
		}
	}
	if err := (Options{Gateway: &agent.Gateway{ID: "meter", URL: "https://gateway.test/route/session"}, Permission: "ask"}).Validate(); err != nil {
		t.Fatal(err)
	}
}

func TestInheritedProfileKeepsCurrentSubscriptionAndPermissionEnvironment(t *testing.T) {
	inherited := []string{"HOME=/fixture/home", "CODEX_HOME=/fixture/current-codex", "CLAUDE_CONFIG_DIR=/fixture/current-claude", "CLAUDE_CODE_OAUTH_TOKEN=fixture-subscription", "ANTHROPIC_API_KEY=fixture-existing-key", "ANTHROPIC_BASE_URL=https://fixture-existing-route.invalid", "OPENAI_API_KEY=fixture-openai-key", "CLAUDE_CODE_DISABLE_BYPASS_PERMISSIONS_MODE=1"}
	for _, provider := range []string{"claude", "codex"} {
		resolved, err := Resolve(Profile{ID: "current", Provider: provider, Isolation: "inherited"}, inherited)
		if err != nil {
			t.Fatal(err)
		}
		if len(resolved) != len(inherited) {
			t.Fatal("inherited profile replaced account context")
		}
		values := map[string]bool{}
		for _, value := range resolved {
			values[value] = true
		}
		for _, value := range inherited {
			if !values[value] {
				t.Fatalf("%s changed inherited environment", provider)
			}
		}
	}
}

func TestDefaultRotationPoolIsSortedBoundedAndProviderScoped(t *testing.T) {
	profiles := []Profile{{ID: "claude-default", Provider: "claude", Isolation: "inherited"}, {ID: "codex-x", Provider: "codex", Isolation: "environment"}}
	for i := 20; i > 0; i-- {
		profiles = append(profiles, Profile{ID: fmt.Sprintf("claude-%02d", i), Provider: "claude", Isolation: "environment"})
	}
	pool := DefaultRotationPool(profiles, Profile{ID: "claude-01", Provider: "claude"}, []string{"claude-02"})
	if len(pool) != MaxRotationProfiles || pool[0] != "claude-03" || pool[len(pool)-1] != "claude-18" {
		t.Fatalf("pool %v must be the first %d other isolated claude subscriptions by ID", pool, MaxRotationProfiles)
	}
	if err := (Options{RotationProfiles: pool}).Validate(); err != nil {
		t.Fatal(err)
	}
	if got := DefaultRotationPool(profiles[:2], profiles[0], nil); len(got) != 0 {
		t.Fatalf("inherited sign-ins and other providers are never in the pool: %v", got)
	}
}
