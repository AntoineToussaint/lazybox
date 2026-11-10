package claude

import (
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
)

func TestGatewayEnvironmentPreservesAuthAndNeverMutatesParent(t *testing.T) {
	parent := []string{"ANTHROPIC_BASE_URL=https://old.test", "ANTHROPIC_API_KEY=provider-secret", "BENCH_TOKEN=gateway-secret"}
	result := GatewayEnv(&agent.Gateway{ID: "meter", URL: "https://meter.test/route/"}, parent)
	joined := strings.Join(result, "\n")
	if strings.Count(joined, "ANTHROPIC_BASE_URL=") != 1 || !strings.Contains(joined, "ANTHROPIC_API_KEY=provider-secret") || !strings.Contains(joined, "ANTHROPIC_BASE_URL=https://meter.test/route") {
		t.Fatal("incorrect gateway or provider credential mapping")
	}
	result = GatewayEnv(&agent.Gateway{ID: "meter", URL: "https://meter.test", TokenEnv: "BENCH_TOKEN"}, parent)
	joined = strings.Join(result, "\n")
	if strings.Contains(joined, "ANTHROPIC_API_KEY=") || !strings.Contains(joined, "ANTHROPIC_AUTH_TOKEN=gateway-secret") {
		t.Fatal("gateway token mode retained competing auth")
	}
	if parent[0] != "ANTHROPIC_BASE_URL=https://old.test" || parent[1] != "ANTHROPIC_API_KEY=provider-secret" {
		t.Fatal("parent environment mutated")
	}
}
