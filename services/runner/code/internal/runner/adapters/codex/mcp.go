package codex

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
)

// listMcpServers reports only inventory labels and tool counts. Provider
// connection details, resource URIs, and tool definitions stay in app-server.
func listMcpServers(ctx context.Context, p caller, thread string) (string, error) {
	var servers []string
	cursor := ""
	for range 8 {
		params := map[string]any{"threadId": thread, "detail": "toolsAndAuthOnly", "limit": 32}
		if cursor != "" {
			params["cursor"] = cursor
		}
		raw, err := p.Call(ctx, "mcpServerStatus/list", params)
		if err != nil {
			return "", fmt.Errorf("could not list MCP servers: %w", err)
		}
		var page struct {
			Data []struct {
				Name          string                     `json:"name"`
				Tools         map[string]json.RawMessage `json:"tools"`
				RuntimeStatus string                     `json:"runtimeStatus"`
			} `json:"data"`
			NextCursor string `json:"nextCursor"`
		}
		if err := json.Unmarshal(raw, &page); err != nil {
			return "", fmt.Errorf("could not read MCP server list: %w", err)
		}
		for _, server := range page.Data {
			label := fmt.Sprintf("%s (%d tools)", server.Name, len(server.Tools))
			switch server.RuntimeStatus {
			case "disabled", "failed", "starting":
				label = server.Name + " (" + server.RuntimeStatus + ")"
			case "authenticationRequired":
				label = server.Name + " (sign-in required)"
			}
			servers = append(servers, label)
		}
		if page.NextCursor == "" {
			if len(servers) == 0 {
				return "No MCP servers are configured for this conversation.", nil
			}
			return "MCP servers: " + strings.Join(servers, " · "), nil
		}
		if page.NextCursor == cursor {
			break
		}
		cursor = page.NextCursor
	}
	return "MCP servers: " + strings.Join(servers, " · ") + " · additional results omitted", nil
}
