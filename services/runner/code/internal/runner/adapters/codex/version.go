package codex

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// TestedVersions is the range of Codex versions this adapter is tested with,
// oldest and newest, inclusive; a patch release of the newest minor counts.
// The adapter speaks experimental, unversioned parts of the app-server
// (experimentalApi, turn/steer, features.multi_agent_v2), so a version
// outside it may break without an error. Widening it means checking the
// schema the new version generates (`codex app-server generate-ts
// --experimental` with a scratch CODEX_HOME) against what this adapter
// sends and reads; RUNTIME_EVIDENCE.md records each check.
const TestedVersions = "0.164.0-0.171"

// UntestedVersionNotice is the event type of the one line a conversation
// shows when its Codex is outside TestedVersions.
const UntestedVersionNotice = "provider_notice"

// version is a dotted release number: 0.169.0.
type version [3]int

func parseVersion(text string) (version, bool) {
	var v version
	parts := strings.SplitN(strings.TrimSpace(text), ".", 3)
	if len(parts) < 2 {
		return v, false
	}
	for i, part := range parts {
		// A pre-release or build suffix (0.170.0-alpha.1) is the release it precedes.
		digits := part
		if end := strings.IndexFunc(part, func(r rune) bool { return r < '0' || r > '9' }); end >= 0 {
			digits = part[:end]
		}
		n, err := strconv.Atoi(digits)
		if err != nil || n < 0 {
			return v, false
		}
		v[i] = n
	}
	return v, true
}

func (v version) less(w version) bool {
	for i := range v {
		if v[i] != w[i] {
			return v[i] < w[i]
		}
	}
	return false
}

// tested reports whether v is within TestedVersions.
func tested(v version) bool {
	oldest, newest, _ := strings.Cut(TestedVersions, "-")
	low, _ := parseVersion(oldest)
	high, _ := parseVersion(newest)
	// The newest bound names a minor: any patch of it is tested.
	high[2] = int(^uint(0) >> 1)
	return !v.less(low) && !high.less(v)
}

// serverVersion is the Codex version an initialize response names: its
// userAgent is "<client name>/<version> (<platform>) …".
func serverVersion(initialize json.RawMessage) (string, bool) {
	var r struct {
		UserAgent string `json:"userAgent"`
	}
	if json.Unmarshal(initialize, &r) != nil {
		return "", false
	}
	_, rest, ok := strings.Cut(r.UserAgent, "/")
	if !ok {
		return "", false
	}
	text, _, _ := strings.Cut(rest, " ")
	if _, ok := parseVersion(text); !ok {
		return "", false
	}
	return text, true
}

// versionNotice is the line a conversation shows when its Codex is outside
// TestedVersions, or false when it is tested or its version unknown.
func versionNotice(initialize json.RawMessage) (agent.Event, bool) {
	text, ok := serverVersion(initialize)
	if !ok {
		return agent.Event{}, false
	}
	v, _ := parseVersion(text)
	if tested(v) {
		return agent.Event{}, false
	}
	oldest, newest, _ := strings.Cut(TestedVersions, "-")
	data, _ := json.Marshal(map[string]string{"provider": "codex", "version": text, "tested": TestedVersions})
	return agent.Event{
		Type:   UntestedVersionNotice,
		Status: "untested_version",
		Text:   fmt.Sprintf("This Codex version (%s) isn't tested with Lazybox yet; some features may not work. Lazybox is tested with Codex %s to %s.", text, strings.TrimSuffix(oldest, ".0"), newest),
		Data:   data,
	}, true
}
