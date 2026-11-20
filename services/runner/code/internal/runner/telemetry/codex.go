package telemetry

func codexTokens(usage map[string]any) (Tokens, bool) {
	input, output, cached := count(usage["inputTokens"]), count(usage["outputTokens"]), count(usage["cachedInputTokens"])
	if input == nil || output == nil || cached == nil || *cached > *input {
		return Tokens{}, false
	}
	return Tokens{Input: number(*input - *cached), Output: output, CacheRead: cached, CacheWrite5m: number(0), CacheWrite1h: number(0)}, true
}

// Codex reports token kinds for the latest request, rather than semantic
// categories like system instructions or MCP tools.
func codexContextParts(last map[string]any, used *int64) []Category {
	tokens, ok := codexTokens(last)
	if !ok || used == nil || *used == 0 || *tokens.Input+*tokens.CacheRead+*tokens.Output != *used {
		return nil
	}
	var categories []Category
	add := func(name string, count int64) {
		if count > 0 {
			categories = append(categories, Category{Name: name, Kind: "used", Included: true, Tokens: number(count)})
		}
	}
	add("uncached_input", *tokens.Input)
	add("cached_input", *tokens.CacheRead)
	output := *tokens.Output
	if reasoning := count(last["reasoningOutputTokens"]); reasoning != nil && *reasoning <= output {
		add("reasoning_output", *reasoning)
		output -= *reasoning
	}
	add("other_output", output)
	return categories
}
