package acp

import (
	"context"
	"encoding/json"
	"fmt"
	"regexp"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// Lazybox's permission modes map onto the agent's own session modes by what
// they are called: ACP leaves modes to each agent ("default", "autoEdit",
// "yolo", "plan", "bypassPermissions"…). A mode that matches is set on the
// agent; whether or not one matches, Lazybox enforces the mode on what the
// agent asks of it (permission requests, file writes, commands).
var modePatterns = map[string]*regexp.Regexp{
	"yolo":      regexp.MustCompile(`(?i)yolo|bypass|full[ _-]?access|danger|never[ _-]?ask|auto[ _-]?approve`),
	"read-only": regexp.MustCompile(`(?i)read[ _-]?only|readonly|^plan`),
	"ask":       regexp.MustCompile(`(?i)^(default|ask|manual|normal|suggest|on[ _-]?request|always[ _-]?ask)$`),
}

// configOption is one of the agent's session config options (ACP's
// standard selectors: mode, model, thought level…).
type configOption struct {
	ID       string          `json:"id"`
	Name     string          `json:"name"`
	Category string          `json:"category"`
	Type     string          `json:"type"`
	Current  json.RawMessage `json:"currentValue"`
	Options  json.RawMessage `json:"options"`
}

type choice struct {
	Value       string `json:"value"`
	Name        string `json:"name"`
	Description string `json:"description,omitempty"`
}

// choices flattens a select option's values, grouped or not.
func (c configOption) choices() []choice {
	var flat []choice
	if json.Unmarshal(c.Options, &flat) == nil && (len(flat) == 0 || flat[0].Value != "") {
		return flat
	}
	var groups []struct {
		Options []choice `json:"options"`
	}
	_ = json.Unmarshal(c.Options, &groups)
	out := []choice{}
	for _, g := range groups {
		out = append(out, g.Options...)
	}
	return out
}

func (c configOption) current() string {
	var v string
	_ = json.Unmarshal(c.Current, &v)
	return v
}

func (s *session) configOptions() []configOption {
	var out []configOption
	_ = json.Unmarshal(s.state.Config, &out)
	return out
}

// selectOption is the select config option of category, if the agent has one.
func (s *session) selectOption(category string) (configOption, bool) {
	for _, o := range s.configOptions() {
		if o.Category == category && (o.Type == "select" || o.Type == "") {
			return o, true
		}
	}
	return configOption{}, false
}

// modeFor is the agent's mode for a Lazybox permission, and whether it is a
// config option rather than a session mode. Empty when none matches.
func (s *session) modeFor(permission string) (id string, option string) {
	pattern := modePatterns[permission]
	if pattern == nil {
		return "", ""
	}
	if o, ok := s.selectOption("mode"); ok {
		for _, c := range o.choices() {
			if pattern.MatchString(c.Value) || pattern.MatchString(c.Name) {
				return c.Value, o.ID
			}
		}
		return "", ""
	}
	if s.state.Modes != nil {
		for _, m := range s.state.Modes.Available {
			if pattern.MatchString(m.ID) || pattern.MatchString(m.Name) {
				return m.ID, ""
			}
		}
	}
	return "", ""
}

// applyPermission records the conversation's permission mode and sets the
// agent's matching mode, when it has one.
func (s *session) applyPermission(ctx context.Context, permission string) error {
	s.mu.Lock()
	s.state.Permission = permission
	mode, option := s.modeFor(permission)
	ref := s.ref
	current := ""
	if s.state.Modes != nil {
		current = s.state.Modes.Current
	}
	if o, ok := s.selectOption("mode"); ok {
		current = o.current()
	}
	s.mu.Unlock()
	if mode == "" || mode == current {
		return nil
	}
	if option != "" {
		return s.setConfig(ctx, option, mode)
	}
	if _, err := s.p.Call(ctx, "session/set_mode", map[string]string{"sessionId": ref, "modeId": mode}); err != nil {
		return fmt.Errorf("%s did not switch to its %q mode: %w", s.title, mode, err)
	}
	s.mu.Lock()
	if s.state.Modes != nil {
		s.state.Modes.Current = mode
	}
	s.mu.Unlock()
	return nil
}

// setConfig sets one config option and keeps the options the agent answers with.
func (s *session) setConfig(ctx context.Context, option, value string) error {
	s.mu.Lock()
	ref := s.ref
	s.mu.Unlock()
	result, err := s.p.Call(ctx, "session/set_config_option", map[string]string{"sessionId": ref, "configId": option, "value": value})
	if err != nil {
		return err
	}
	var r struct {
		Config json.RawMessage `json:"configOptions"`
	}
	if json.Unmarshal(result, &r) == nil && present(r.Config) {
		s.mu.Lock()
		s.state.Config = r.Config
		s.mu.Unlock()
	}
	return nil
}

// modelChoices is the models the agent offers, or nil when it offers no
// choice. The caller holds s.mu.
func (s *session) modelChoices() []choice {
	if o, ok := s.selectOption("model"); ok {
		return o.choices()
	}
	if s.state.Models != nil {
		out := []choice{}
		for _, m := range s.state.Models.Available {
			out = append(out, choice{Value: m.ID, Name: m.Name, Description: m.Description})
		}
		return out
	}
	return nil
}

// setModel switches the agent's model: its model config option, else the
// pre-standard session/set_model of agents that report `models`.
func (s *session) setModel(ctx context.Context, model string) error {
	s.mu.Lock()
	choices := s.modelChoices()
	option, standard := s.selectOption("model")
	ref := s.ref
	s.mu.Unlock()
	if choices == nil {
		return fmt.Errorf("%w: %s does not let Lazybox choose its model", agent.ErrUnsupported, s.title)
	}
	value := ""
	names := []string{}
	for _, c := range choices {
		names = append(names, c.Value)
		if c.Value == model || strings.EqualFold(c.Name, model) {
			value = c.Value
		}
	}
	if value == "" {
		return fmt.Errorf("%w: %s is not one of %s's models (%s)", agent.ErrInvalidModel, model, s.title, strings.Join(names, ", "))
	}
	var err error
	if standard {
		err = s.setConfig(ctx, option.ID, value)
	} else {
		_, err = s.p.Call(ctx, "session/set_model", map[string]string{"sessionId": ref, "modelId": value})
		if err == nil {
			s.mu.Lock()
			s.state.Models.Current = value
			s.mu.Unlock()
		}
	}
	if err != nil {
		return err
	}
	s.mu.Lock()
	s.state.Model = value
	s.mu.Unlock()
	return nil
}

// catalogEvent is the agent's models as a command_catalog event, for the
// model picker and /model.
func (s *session) catalogEvent() (agent.Event, bool) {
	s.mu.Lock()
	choices := s.modelChoices()
	s.mu.Unlock()
	if len(choices) == 0 {
		return agent.Event{}, false
	}
	models := []map[string]string{}
	for _, c := range choices {
		models = append(models, map[string]string{"value": c.Value, "displayName": c.Name, "description": c.Description})
	}
	data, _ := json.Marshal(map[string]any{"models": models})
	return agent.Event{Type: "command_catalog", AgentID: "root", Data: data}, true
}

func (s *session) emitCatalog() {
	if ev, ok := s.catalogEvent(); ok {
		s.emit(ev)
	}
}
