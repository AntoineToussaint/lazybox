package availability

import (
	"fmt"
	"regexp"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

var claudeVersionedShorthand = regexp.MustCompile(`^(?:opus|sonnet|haiku|fable)[ .-]+[0-9]+(?:[ .-]+[0-9]+)*$`)

// ResolveModel maps known display names and versioned shorthand to provider
// IDs. Full IDs and provider aliases can come from an account's own catalog.
func ResolveModel(provider, requested string) (string, error) {
	model := strings.TrimSpace(requested)
	if model == "" {
		return "", nil
	}
	key := modelKey(model)
	for _, choice := range BuiltinModels[provider] {
		if key == modelKey(choice.Value) || key == modelKey(choice.DisplayName) {
			return choice.Value, nil
		}
	}
	if provider == "claude" && claudeVersionedShorthand.MatchString(strings.ToLower(model)) {
		return "", fmt.Errorf("%w: %q is not a known Claude model name; choose from the model picker or use the full provider ID", agent.ErrInvalidModel, model)
	}
	return model, agent.ValidModel(model)
}

func modelKey(name string) string {
	return strings.Join(strings.FieldsFunc(strings.ToLower(name), func(r rune) bool {
		return r == ' ' || r == '-' || r == '.'
	}), "-")
}
