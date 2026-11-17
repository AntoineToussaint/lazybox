package process

import (
	"bytes"
	"encoding/json"
	"fmt"
	"reflect"
	"strings"
	"time"

	"gopkg.in/yaml.v3"
)

// StringList is one string or a list of strings: `on: task.reported` and
// `on: [task.question, task.blocked]` are both valid.
type StringList []string

func (l *StringList) UnmarshalYAML(n *yaml.Node) error {
	if n.Kind == yaml.ScalarNode {
		*l = StringList{n.Value}
		return nil
	}
	var values []string
	if err := n.Decode(&values); err != nil {
		return err
	}
	*l = values
	return nil
}

func (l StringList) MarshalYAML() (any, error) {
	if len(l) == 1 {
		return l[0], nil
	}
	return []string(l), nil
}

func (l StringList) MarshalJSON() ([]byte, error) {
	if len(l) == 1 {
		return json.Marshal(l[0])
	}
	return json.Marshal([]string(l))
}

func (l *StringList) UnmarshalJSON(data []byte) error {
	var one string
	if json.Unmarshal(data, &one) == nil {
		*l = StringList{one}
		return nil
	}
	var many []string
	if err := json.Unmarshal(data, &many); err != nil {
		return err
	}
	*l = many
	return nil
}

func (StringList) jsonSchema() map[string]any {
	return map[string]any{"oneOf": []any{
		map[string]any{"type": "string"},
		map[string]any{"type": "array", "items": map[string]any{"type": "string"}, "minItems": 1},
	}}
}

// Duration is a Go duration written as a string: 30m, 10h, 1h30m.
type Duration time.Duration

const durationPattern = `^([0-9]+(\.[0-9]+)?(h|m|s|ms))+$`

func parseDuration(text string) (Duration, error) {
	d, err := time.ParseDuration(text)
	if err != nil || d <= 0 {
		return 0, fmt.Errorf("%q is not a positive duration such as 30m or 10h", text)
	}
	return Duration(d), nil
}

// String writes the shortest form: 10h, 30m, 1h30m, 45s.
func (d Duration) String() string {
	if d == 0 {
		return ""
	}
	rest := time.Duration(d)
	var b strings.Builder
	for _, unit := range []struct {
		size time.Duration
		name string
	}{{time.Hour, "h"}, {time.Minute, "m"}, {time.Second, "s"}, {time.Millisecond, "ms"}} {
		if n := rest / unit.size; n > 0 {
			fmt.Fprintf(&b, "%d%s", n, unit.name)
			rest -= n * unit.size
		}
	}
	if b.Len() == 0 {
		return time.Duration(d).String()
	}
	return b.String()
}

func (d *Duration) UnmarshalYAML(n *yaml.Node) error {
	parsed, err := parseDuration(n.Value)
	if err != nil {
		return err
	}
	*d = parsed
	return nil
}

func (d Duration) MarshalYAML() (any, error) { return d.String(), nil }

func (d Duration) MarshalJSON() ([]byte, error) { return json.Marshal(d.String()) }

func (d *Duration) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	parsed, err := parseDuration(text)
	if err != nil {
		return err
	}
	*d = parsed
	return nil
}

func (Duration) jsonSchema() map[string]any {
	return map[string]any{"type": "string", "pattern": durationPattern}
}

// Map is a mapping that keeps the order its keys were written in, so roles
// and gates are shown, explained and fired in file order. A nil value in an
// overlay removes the key.
type Map[T any] struct {
	Keys   []string
	Values map[string]T
}

// Get returns the value for key.
func (m Map[T]) Get(key string) (T, bool) {
	v, ok := m.Values[key]
	return v, ok
}

// Len is the number of keys.
func (m Map[T]) Len() int { return len(m.Keys) }

// Set adds or replaces key, keeping its position when it exists.
func (m *Map[T]) Set(key string, value T) {
	if m.Values == nil {
		m.Values = map[string]T{}
	}
	if _, ok := m.Values[key]; !ok {
		m.Keys = append(m.Keys, key)
	}
	m.Values[key] = value
}

// Delete removes key.
func (m *Map[T]) Delete(key string) {
	if _, ok := m.Values[key]; !ok {
		return
	}
	delete(m.Values, key)
	for i, k := range m.Keys {
		if k == key {
			m.Keys = append(m.Keys[:i:i], m.Keys[i+1:]...)
			break
		}
	}
}

func (m Map[T]) IsZero() bool { return len(m.Keys) == 0 }

func (m *Map[T]) UnmarshalYAML(n *yaml.Node) error {
	if n.Kind != yaml.MappingNode {
		return fmt.Errorf("line %d: expected a mapping", n.Line)
	}
	*m = Map[T]{}
	for i := 0; i+1 < len(n.Content); i += 2 {
		var value T
		if err := n.Content[i+1].Decode(&value); err != nil {
			return err
		}
		m.Set(n.Content[i].Value, value)
	}
	return nil
}

func (m Map[T]) MarshalYAML() (any, error) {
	out := &yaml.Node{Kind: yaml.MappingNode}
	for _, k := range m.Keys {
		value := &yaml.Node{}
		if err := value.Encode(m.Values[k]); err != nil {
			return nil, err
		}
		out.Content = append(out.Content, &yaml.Node{Kind: yaml.ScalarNode, Value: k}, value)
	}
	return out, nil
}

func (m Map[T]) MarshalJSON() ([]byte, error) {
	var b bytes.Buffer
	b.WriteByte('{')
	for i, k := range m.Keys {
		if i > 0 {
			b.WriteByte(',')
		}
		key, err := json.Marshal(k)
		if err != nil {
			return nil, err
		}
		value, err := json.Marshal(m.Values[k])
		if err != nil {
			return nil, err
		}
		b.Write(key)
		b.WriteByte(':')
		b.Write(value)
	}
	b.WriteByte('}')
	return b.Bytes(), nil
}

func (m *Map[T]) UnmarshalJSON(data []byte) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	token, err := decoder.Token()
	if err != nil {
		return err
	}
	if token != json.Delim('{') {
		return fmt.Errorf("expected an object")
	}
	*m = Map[T]{}
	for decoder.More() {
		token, err = decoder.Token()
		if err != nil {
			return err
		}
		var value T
		if err = decoder.Decode(&value); err != nil {
			return err
		}
		m.Set(token.(string), value)
	}
	_, err = decoder.Token()
	return err
}

// The bare string form of an action.
const enterNextPhase = "enter_next_phase"

type plainAction Action

func (a *Action) UnmarshalYAML(n *yaml.Node) error {
	if n.Kind == yaml.ScalarNode {
		if n.Value != enterNextPhase {
			return fmt.Errorf("line %d: %q is not an action; the only bare action is %s", n.Line, n.Value, enterNextPhase)
		}
		*a = Action{EnterNextPhase: true}
		return nil
	}
	return n.Decode((*plainAction)(a))
}

func (a Action) MarshalYAML() (any, error) {
	if a.Equal(Action{EnterNextPhase: true}) {
		return enterNextPhase, nil
	}
	return plainAction(a), nil
}

func (a Action) MarshalJSON() ([]byte, error) {
	if a.Equal(Action{EnterNextPhase: true}) {
		return json.Marshal(enterNextPhase)
	}
	return json.Marshal(plainAction(a))
}

func (a *Action) UnmarshalJSON(data []byte) error {
	var bare string
	if json.Unmarshal(data, &bare) == nil {
		if bare != enterNextPhase {
			return fmt.Errorf("%q is not an action", bare)
		}
		*a = Action{EnterNextPhase: true}
		return nil
	}
	return json.Unmarshal(data, (*plainAction)(a))
}

// Equal reports whether two actions are the same.
func (a Action) Equal(b Action) bool { return reflect.DeepEqual(a, b) }

// bareForms are the strings that stand for a whole action.
func (Action) bareForms() []string { return []string{enterNextPhase} }
