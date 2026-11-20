package codex

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

// maxModels bounds what one listing keeps, as the Claude worker's catalog does.
const maxModels = 64

type caller interface {
	Call(ctx context.Context, method string, params any) (json.RawMessage, error)
}

// listModels asks an app-server for the models its picker offers
// (`model/list`), page by page, leaving out the hidden ones. Codex answers
// from its own catalog, refreshed for the signed-in account, so a session's
// app-server lists what that subscription can use.
func listModels(ctx context.Context, p caller) ([]agent.ModelChoice, error) {
	out := []agent.ModelChoice{}
	seen := map[string]bool{}
	cursor := ""
	for page := 0; page < 8 && len(out) < maxModels; page++ {
		params := map[string]any{}
		if cursor != "" {
			params["cursor"] = cursor
		}
		raw, err := p.Call(ctx, "model/list", params)
		if err != nil {
			return nil, err
		}
		var response struct {
			Data []struct {
				ID                        string `json:"id"`
				Model                     string `json:"model"`
				DisplayName               string `json:"displayName"`
				Description               string `json:"description"`
				Hidden                    bool   `json:"hidden"`
				SupportedReasoningEfforts []struct {
					Effort string `json:"reasoningEffort"`
				} `json:"supportedReasoningEfforts"`
			} `json:"data"`
			NextCursor *string `json:"nextCursor"`
		}
		if err = json.Unmarshal(raw, &response); err != nil {
			return nil, err
		}
		for _, m := range response.Data {
			// `model` is what turn/start takes; `id` names the preset.
			value := strings.TrimSpace(m.Model)
			if value == "" {
				value = strings.TrimSpace(m.ID)
			}
			if m.Hidden || value == "" || seen[value] || agent.ValidModel(value) != nil {
				continue
			}
			seen[value] = true
			name := strings.TrimSpace(m.DisplayName)
			if name == "" {
				name = value
			}
			var efforts []string
			for _, level := range m.SupportedReasoningEfforts {
				if level.Effort != "" && agent.ValidEffort("codex", level.Effort) == nil {
					efforts = append(efforts, level.Effort)
				}
			}
			out = append(out, agent.ModelChoice{EffortLevels: efforts, Value: value, DisplayName: name, Description: strings.TrimSpace(m.Description)})
			if len(out) >= maxModels {
				break
			}
		}
		if response.NextCursor == nil || *response.NextCursor == "" || *response.NextCursor == cursor {
			break
		}
		cursor = *response.NextCursor
	}
	return out, nil
}

// catalogEvent is models as the command_catalog event the model picker and
// /model read; false when there is nothing to say.
func catalogEvent(models []agent.ModelChoice) (agent.Event, bool) {
	if len(models) == 0 {
		return agent.Event{}, false
	}
	data, err := json.Marshal(map[string]any{"models": models})
	if err != nil {
		return agent.Event{}, false
	}
	return agent.Event{Type: "command_catalog", AgentID: "root", Data: data}, true
}

// readyEvent says the model a new thread runs on, as the Claude worker's
// session_ready does; false when Codex named none.
func readyEvent(model string) (agent.Event, bool) {
	if strings.TrimSpace(model) == "" {
		return agent.Event{}, false
	}
	data, err := json.Marshal(map[string]string{"model": model})
	if err != nil {
		return agent.Event{}, false
	}
	return agent.Event{Type: "session_ready", AgentID: "root", Data: data}, true
}

// withModel adds the model in force to a usage notification's params, which
// name none; params it cannot read, or that name one, pass as they are.
func withModel(params json.RawMessage, model string) json.RawMessage {
	if model == "" {
		return params
	}
	var fields map[string]json.RawMessage
	if json.Unmarshal(params, &fields) != nil || fields == nil {
		return params
	}
	if _, named := fields["model"]; named {
		return params
	}
	fields["model"], _ = json.Marshal(model)
	data, err := json.Marshal(fields)
	if err != nil {
		return params
	}
	return data
}

// reportModels lists the session's models on its own app-server and says
// them once, beside the conversation: a slow or failed listing never holds up
// the session, and /model then falls back to the Runner's list.
func (s *session) reportModels() {
	ctx, cancel := context.WithTimeout(s.ctx, 20*time.Second)
	defer cancel()
	models, err := listModels(ctx, s.p)
	if err != nil {
		return
	}
	if ev, ok := catalogEvent(models); ok {
		s.emit(ev)
	}
}

// ListModels starts an app-server only to list its models, then stops it.
// The Runner lists Codex's models this way for its provider report, with a
// scratch CODEX_HOME: what the installed Codex offers, read without any
// account, so it never touches a sign-in.
func ListModels(ctx context.Context, binary string, env []string, cwd string) ([]agent.ModelChoice, error) {
	if binary == "" {
		binary = "codex"
	}
	ctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	p, err := jsonl.Start(ctx, binary, []string{"app-server", "--stdio"}, env, cwd)
	if err != nil {
		return nil, err
	}
	defer p.Close()
	if _, err = p.Call(ctx, "initialize", map[string]any{"clientInfo": map[string]string{"name": "lazybox_models", "version": "0.1.0"}}); err != nil {
		return nil, err
	}
	if err = p.Notify("initialized", map[string]any{}); err != nil {
		return nil, err
	}
	models, err := listModels(ctx, p)
	if err == nil && len(models) == 0 {
		err = errors.New("Codex listed no models")
	}
	return models, err
}
