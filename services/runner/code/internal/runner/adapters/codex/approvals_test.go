package codex

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestApprovalsOfferCodexsOwnAlwaysChoices(t *testing.T) {
	command := json.RawMessage(`{"command":"make test","proposedExecpolicyAmendment":["make","test"],"proposedNetworkPolicyAmendments":[{"host":"registry.npmjs.org","action":"allow"},{"host":"evil.test","action":"deny"}]}`)
	choices := alwaysChoices("item/commandExecution/requestApproval", command)
	ids := []string{}
	for _, c := range choices {
		ids = append(ids, c.ID+"/"+c.Scope)
	}
	if strings.Join(ids, " ") != "prefix/prefix network:0/host session/conversation" {
		t.Fatalf("choices: %v", ids)
	}
	if choices[0].Label != "commands starting with `make test`" || choices[1].Label != "network access to registry.npmjs.org" {
		t.Fatalf("labels: %+v", choices)
	}
	if got := alwaysChoices("item/fileChange/requestApproval", json.RawMessage(`{"grantRoot":"/w/src"}`)); len(got) != 1 || got[0].ID != "session" || got[0].Label != "changes under /w/src" {
		t.Fatalf("file change: %+v", got)
	}
	if got := alwaysChoices("item/permissions/requestApproval", json.RawMessage(`{}`)); got != nil {
		t.Fatalf("a permissions request asking for nothing has no always: %+v", got)
	}
	var data map[string]json.RawMessage
	if err := json.Unmarshal(withAlways("item/commandExecution/requestApproval", command), &data); err != nil || len(data["always"]) == 0 || string(data["command"]) != `"make test"` {
		t.Fatalf("data keeps the native params and adds always: %s %v", data, err)
	}
}

func TestAlwaysChoicesBecomeNativeDecisions(t *testing.T) {
	params := json.RawMessage(`{"command":"make test","proposedExecpolicyAmendment":["make","test"],"proposedNetworkPolicyAmendments":[{"host":"registry.npmjs.org","action":"allow"}]}`)
	method := "item/commandExecution/requestApproval"
	cases := map[string]string{
		"":          `{"decision":"accept"}`,
		"session":   `{"decision":"acceptForSession"}`,
		"prefix":    `{"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["make","test"]}}}`,
		"network:0": `{"decision":{"applyNetworkPolicyAmendment":{"network_policy_amendment":{"host":"registry.npmjs.org","action":"allow"}}}}`,
	}
	for always, want := range cases {
		reply, err := nativeResponse(method, params, agent.Operation{Kind: agent.Approve, Decision: "accept", Always: always})
		got, _ := json.Marshal(reply)
		if err != nil || string(got) != want {
			t.Fatalf("always %q: %s %v", always, got, err)
		}
	}
	if _, err := nativeResponse(method, params, agent.Operation{Kind: agent.Approve, Decision: "accept", Always: "everything"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("an always the request did not offer: %v", err)
	}
	if _, err := nativeResponse("item/fileChange/requestApproval", json.RawMessage(`{}`), agent.Operation{Kind: agent.Approve, Decision: "accept", Always: "prefix"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("a file change has no prefix rule: %v", err)
	}
}

// Through the app-server: the request offers the amendment Codex proposed,
// and accepting it sends Codex its own acceptWithExecpolicyAmendment.
func TestAlwaysAllowReachesTheAppServer(t *testing.T) {
	t.Setenv("FIXTURE_NATIVE_APPROVAL", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: fixtureRoot(t)}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	_, _ = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "clean"})
	approval := receive(t, s, "approval")
	var data struct{ Always []AlwaysChoice }
	if json.Unmarshal(approval.Data, &data) != nil || len(data.Always) != 2 || data.Always[0].ID != "prefix" || data.Always[1].ID != "session" {
		t.Fatalf("offered: %s", approval.Data)
	}
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "bad", Kind: agent.Approve, RequestID: approval.RequestID, Decision: "accept", Always: "network:3"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("an unoffered choice is refused: %v", err)
	}
	// The refused choice left the request open: the user can still answer it.
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "ok", Kind: agent.Approve, RequestID: approval.RequestID, Decision: "accept", Always: "prefix"}); err != nil {
		t.Fatal(err)
	}
	if reply := receive(t, s, "message_delta"); reply.Text != `decision={"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["rm","-rf"]}}` {
		t.Fatalf("app-server got %q", reply.Text)
	}
}

// Codex's decline carries no text, so the user's reason follows as a steer
// of the same turn, in the user's words.
func TestDeclineReasonReachesTheAgent(t *testing.T) {
	t.Setenv("FIXTURE_NATIVE_APPROVAL", "1")
	t.Setenv("FIXTURE_KEEP_TURN", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: fixtureRoot(t)}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	_, _ = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "clean"})
	approval := receive(t, s, "approval")
	r, err := s.Apply(context.Background(), agent.Operation{ID: "no", Kind: agent.Approve, RequestID: approval.RequestID, Decision: "decline", Reason: "use make clean instead"})
	if err != nil || r.Error != "" {
		t.Fatalf("decline: %+v %v", r, err)
	}
	if reply := receive(t, s, "message_delta"); reply.Text != "decision=decline" {
		t.Fatalf("app-server got %q", reply.Text)
	}
	if steer := receive(t, s, "message_delta"); steer.Text != "steer=I declined running `rm -rf build`. use make clean instead" {
		t.Fatalf("reason: %q", steer.Text)
	}
}

// Codex asking for more access (item/permissions/requestApproval): Allow
// grants exactly what it asked, for the turn or, always, the conversation;
// Deny grants nothing; a request asking for nothing Lazybox can name is
// marked ungrantable, so the card offers only Deny.
func TestPermissionsRequestsGrantWhatWasAsked(t *testing.T) {
	params := json.RawMessage(`{"threadId":"t","turnId":"u","itemId":"i","environmentId":null,"startedAtMs":1,"cwd":"/w","reason":"needs the registry","permissions":{"network":{"enabled":true},"fileSystem":{"read":null,"write":["/tmp/out"],"entries":[{"path":{"type":"path","path":"/opt/cache"},"access":"read"},{"path":{"type":"path","path":"/etc"},"access":"deny"}]}}}`)
	choices := alwaysChoices(permissionsMethod, params)
	if len(choices) != 1 || choices[0].ID != "session" || choices[0].Label != "network access and writing /tmp/out and reading /opt/cache" {
		t.Fatalf("choices: %+v", choices)
	}
	var data map[string]json.RawMessage
	if err := json.Unmarshal(withAlways(permissionsMethod, params), &data); err != nil || string(data["grantable"]) != "true" || string(data["title"]) != `"More access: network access and writing /tmp/out and reading /opt/cache"` || len(data["always"]) == 0 {
		t.Fatalf("data: %s %v", data, err)
	}
	granted := `{"fileSystem":{"read":null,"write":["/tmp/out"],"entries":[{"path":{"type":"path","path":"/opt/cache"},"access":"read"},{"path":{"type":"path","path":"/etc"},"access":"deny"}]},"network":{"enabled":true}}`
	for always, want := range map[string]string{
		"":        `{"permissions":` + granted + `,"scope":"turn"}`,
		"session": `{"permissions":` + granted + `,"scope":"session"}`,
	} {
		reply, err := nativeResponse(permissionsMethod, params, agent.Operation{Kind: agent.Approve, Decision: "accept", Always: always})
		got, _ := json.Marshal(reply)
		if err != nil || string(got) != want {
			t.Fatalf("always %q: %s %v", always, got, err)
		}
	}
	for _, decision := range []string{"decline", "cancel"} {
		reply, err := nativeResponse(permissionsMethod, params, agent.Operation{Kind: agent.Approve, Decision: decision})
		if got, _ := json.Marshal(reply); err != nil || string(got) != `{"permissions":{},"scope":"turn"}` {
			t.Fatalf("%s: %s %v", decision, got, err)
		}
	}
	if _, err := nativeResponse(permissionsMethod, params, agent.Operation{Kind: agent.Approve, Decision: "accept", Always: "prefix"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("an always the request did not offer: %v", err)
	}
	// Network off and nothing to read or write: nothing to grant.
	empty := json.RawMessage(`{"permissions":{"network":{"enabled":false},"fileSystem":{"read":[],"write":null}}}`)
	if _, err := nativeResponse(permissionsMethod, empty, agent.Operation{Kind: agent.Approve, Decision: "accept"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("granted nothing as if it were something: %v", err)
	}
	data = nil
	if err := json.Unmarshal(withAlways(permissionsMethod, empty), &data); err != nil || string(data["grantable"]) != "false" || data["always"] != nil || data["title"] != nil {
		t.Fatalf("ungrantable data: %s %v", data, err)
	}
}

// Through the app-server: Allow on a permissions request reaches Codex as the
// access it asked for, at the scope chosen.
func TestPermissionsAllowReachesTheAppServer(t *testing.T) {
	t.Setenv("FIXTURE_PERMISSIONS_APPROVAL", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: fixtureRoot(t)}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	_, _ = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "fetch"})
	approval := receive(t, s, "approval")
	if approval.Text != permissionsMethod {
		t.Fatalf("request: %+v", approval)
	}
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "ok", Kind: agent.Approve, RequestID: approval.RequestID, Decision: "accept", Always: "session"}); err != nil {
		t.Fatal(err)
	}
	if reply := receive(t, s, "message_delta"); reply.Text != `permissions={"permissions":{"network":{"enabled":true}},"scope":"session"}` {
		t.Fatalf("app-server got %q", reply.Text)
	}
}

func TestOffersCommandAccept(t *testing.T) {
	for params, want := range map[string]bool{
		`{}`: true,
		`{"availableDecisions":["accept","decline"]}`:                             true,
		`{"availableDecisions":[]}`:                                               false,
		`{"availableDecisions":["decline",{"acceptWithExecpolicyAmendment":{}}]}`: false,
	} {
		if got := offersCommandAccept(json.RawMessage(params)); got != want {
			t.Errorf("%s: got %v, want %v", params, got, want)
		}
	}
}
