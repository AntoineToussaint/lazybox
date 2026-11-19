package availability

import (
	"errors"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestResolveModel(t *testing.T) {
	for _, tt := range []struct{ provider, input, want string }{
		{"claude", "Fable 5.2", "claude-fable-5-2"},
		{"claude", "fable-5.2", "claude-fable-5-2"},
		{"claude", "claude-fable-5.2", "claude-fable-5-2"},
		{"claude", " SONNET-5.5 ", "claude-sonnet-5-5"},
		{"claude", "Haiku 5.5", "claude-haiku-5-5"},
		{"claude", "opus", "opus"},
		{"claude", "claude-sonnet-4-6", "claude-sonnet-4-6"},
		{"codex", "GPT-6.1 Terra", "gpt-6.1-terra"},
		{"codex", "GPT 6.2 Sol", "gpt-6.2-sol"},
		{"acp-local", "my-model", "my-model"},
		{"claude", "", ""},
	} {
		if got, err := ResolveModel(tt.provider, tt.input); err != nil || got != tt.want {
			t.Errorf("%s %q: got %q, %v; want %q", tt.provider, tt.input, got, err, tt.want)
		}
	}
	for _, input := range []string{"fable-5.99", "a made up model", "broken\ncommand"} {
		if _, err := ResolveModel("claude", input); !errors.Is(err, agent.ErrInvalidModel) {
			t.Errorf("%q: expected invalid model, got %v", input, err)
		}
	}
}
