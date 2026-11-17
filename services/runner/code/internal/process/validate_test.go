package process

import (
	"fmt"
	"strings"
	"testing"
	"testing/fstest"
)

// repo resolves a repository file over the default user layer.
func repo(t *testing.T, yaml string, opts Options) *Effective {
	t.Helper()
	return Resolve(nil, Parse(LayerRepository, RepositoryFile, []byte(yaml)), opts)
}

// Each invalid file is reported at the path (and, for structural errors, the
// line) that is wrong, with a message saying why.
func TestValidationErrors(t *testing.T) {
	for _, tc := range []struct {
		name, file string
		path, text string
		line       int // 0: do not check
	}{
		{"unknown key", "version: 1\nrolez: {}\n", "rolez", "unknown key", 2},
		{"unknown nested key", "version: 1\nroles:\n  worker:\n    modle: x\n", "roles.worker.modle", "unknown key", 4},
		{"wrong type", "version: 1\nautonomy: high\n", "autonomy", "expected an integer", 2},
		{"off enum", "version: 1\nroles:\n  worker: {permission: root}\n", "roles.worker.permission", `"root" is not one of read_only, ask, yolo`, 3},
		{"bad duration", "version: 1\nbudgets:\n  mission: {wall: 10 hours}\n", "budgets.mission.wall", "not a duration", 3},
		{"out of range", "version: 1\nautonomy: 9\n", "autonomy", "above the maximum 5", 2},
		{"version missing", "name: x\n", "", "version is required", 1},
		{"version unknown", "version: 2\n", "version", "above the maximum 1", 1},
		{"duplicate key", "version: 1\nname: a\nname: b\n", "name", "duplicate key", 3},
		{"bad action string", "version: 1\nrules:\n  - {on: task.reported, do: start_reviewer}\n", "rules[0].do", `"start_reviewer" is not one of enter_next_phase`, 3},
		{"repository sets a ceiling", "version: 1\nceiling: {autonomy: 5}\n", "ceiling", "only your user process file may set a ceiling", 2},
		{"unknown preset", "version: 1\nextends: dark-factory\n", "extends", `unknown preset "dark-factory"`, 2},
		{"unknown role in action", "version: 1\nrules:\n  - {on: task.reported, do: {start: verifierr}}\n", "rules[0].do.start", `no role "verifierr"`, 3},
		{"unknown event", "version: 1\nrules:\n  - {on: task.reportd, do: {request_gate: reviewed}}\n", "rules[0].on", `unknown event type "task.reportd"`, 3},
		{"unknown event in a list", "version: 1\nnotifications:\n  - {on: [task.question, gate.opened], to: [inbox]}\n", "notifications[0].on[1]", `unknown event type "gate.opened"`, 3},
		{"cel syntax", "version: 1\nrules:\n  - {on: task.reported, if: \"task.(\", do: {request_gate: reviewed}}\n", "rules[0].if", "Syntax error", 3},
		{"cel variable of another event", "version: 1\nrules:\n  - {on: task.reported, if: \"gate.id == 'x'\", do: {request_gate: reviewed}}\n", "rules[0].if", "undeclared reference to 'gate'", 3},
		{"cel not a bool", "version: 1\nrules:\n  - {on: task.reported, if: \"1 + 2\", do: {request_gate: reviewed}}\n", "rules[0].if", "must be true or false", 3},
		{"gate condition", "version: 1\ngates:\n  reported: {kind: rule, when: \"verdict.passed\"}\n", "gates.reported.when", "undeclared reference to 'verdict'", 3},
		{"unreachable phase", "version: 1\nrules:\n  - {on: task.reported, do: {request_gate: reviewed}}\n", "phases[1]", `phase "review" is unreachable`, 0},
		{"phase exit missing", "version: 1\nphases:\n  - {id: build, exit: shipped}\n", "phases[0].exit", `no gate "shipped"`, 3},
		{"phase listed twice", "version: 1\nphases:\n  - {id: build, exit: reported}\n  - {id: build, exit: reviewed}\n", "phases[1].id", "listed twice", 4},
		{"gate pass scope", "version: 1\nroles:\n  worker: {scopes: [task.report, gate.pass]}\n", "roles.worker.scopes[1]", "no role may pass a gate", 3},
		{"unknown scope", "version: 1\nroles:\n  worker: {scopes: [repo.write]}\n", "roles.worker.scopes[0]", `unknown scope "repo.write"`, 3},
		{"spawn of a missing role", "version: 1\nroles:\n  worker: {scopes: [session.spawn:helper]}\n", "roles.worker.scopes[0]", `no role "helper" to spawn`, 3},
		{"spawn without a role", "version: 1\nroles:\n  worker: {scopes: [session.spawn]}\n", "roles.worker.scopes[0]", "session.spawn:<role>", 3},
		{"parameter on a plain scope", "version: 1\nroles:\n  worker: {scopes: [\"task.report:x\"]}\n", "roles.worker.scopes[0]", "takes no parameter", 3},
		{"holdout without holdout", "version: 1\nroles:\n  reviewer: {scopes: [holdout.read]}\n", "roles.reviewer.scopes", "without a holdout section", 3},
		{"builder reads holdout", "version: 1\nholdout: {store: runner, readers: [worker]}\nroles:\n  worker: {scopes: [task.report, holdout.read]}\n", "roles.worker.scopes", "a builder must never see the holdout", 4},
		{"scenarios without holdout.read", "version: 1\nroles:\n  reviewer: {context: [diff, scenarios]}\n", "roles.reviewer.context", "only a role with holdout.read", 3},
		{"rule on a timer", "version: 1\nrules:\n  - {on: timer, do: {escalate: late}}\n", "rules[0].on", "no period on a rule", 3},
		{"timer without every", "version: 1\nroles:\n  reviewer:\n    triggers: [{on: timer}]\n", "roles.reviewer.triggers[0]", "needs every", 4},
		{"every without timer", "version: 1\nroles:\n  reviewer:\n    triggers: [{on: task.reported, every: 5m}]\n", "roles.reviewer.triggers[0].every", "only to on: timer", 4},
		{"human gate with a role", "version: 1\ngates:\n  reviewed: {kind: human, role: reviewer}\n", "gates.reviewed.role", "only the owner passes a human gate", 3},
		{"rule gate without when", "version: 1\ngates:\n  shipped: {kind: rule}\n", "gates.shipped", "needs when", 3},
		{"agent gate without role", "version: 1\ngates:\n  reviewed: {kind: agent, when: \"true\"}\n", "gates.reviewed", "needs role", 3},
		{"two verbs", "version: 1\nrules:\n  - {on: task.reported, do: {start: reviewer, request_gate: reviewed}}\n", "rules[0].do", "exactly one verb", 3},
		{"send without content", "version: 1\nrules:\n  - {on: task.reported, do: {send: worker}}\n", "rules[0].do", "exactly one of snippet or text", 3},
		{"assign without create_task", "version: 1\nrules:\n  - {on: task.reported, do: {start: reviewer, assign: worker}}\n", "rules[0].do.assign", "goes with create_task", 3},
		{"exec escapes the repository", "version: 1\nrules:\n  - {on: task.reported, do: {exec: ../../bin/x.sh}}\n", "rules[0].do.exec", "inside the repository", 3},
		{"exec absolute", "version: 1\nrules:\n  - {on: task.reported, do: {exec: /usr/bin/x}}\n", "rules[0].do.exec", "inside the repository", 3},
		{"two instruction sources", "version: 1\nroles:\n  worker:\n    instructions: {file: a.md, text: hi}\n", "roles.worker.instructions", "exactly one of file, snippet, text or agent", 4},
		{"instruction file escapes", "version: 1\nroles:\n  worker:\n    instructions: {file: ../secrets.md}\n", "roles.worker.instructions.file", "inside the repository", 4},
		{"other than a missing role", "version: 1\nroles:\n  reviewer: {provider: other_than_builder}\n", "roles.reviewer.provider", `no role "builder"`, 3},
		{"unknown provider", "version: 1\nroles:\n  reviewer: {provider: gemini}\n", "roles.reviewer.provider", `unknown provider "gemini"`, 3},
		{"new role without provider", "version: 1\nroles:\n  scribe: {scopes: [status.report]}\n", "roles.scribe", "provider is required", 3},
		{"role retries", "version: 1\nroles:\n  worker: {budget: {retries: 2}}\n", "roles.worker.budget.retries", "belong to tasks", 3},
		{"role budget above the mission", "version: 1\nroles:\n  worker: {budget: {usd: 30}}\n", "roles.worker.budget.usd", "above the mission budget (20)", 3},
		{"task budget above the mission", "version: 1\nbudgets:\n  task: {usd: 25}\n", "budgets.task.usd", "above", 3},
		{"snippets for a missing phase", "version: 1\nsnippets:\n  suggest: {ship: [pr]}\n", "snippets.suggest.ship", `no phase "ship"`, 3},
	} {
		t.Run(tc.name, func(t *testing.T) {
			e := repo(t, tc.file, Options{})
			if e.Valid() {
				t.Fatalf("accepted:\n%s", tc.file)
			}
			for _, i := range e.Issues.Errors() {
				if i.Path == tc.path && strings.Contains(i.Message, tc.text) {
					if tc.line != 0 && (i.Line != tc.line || i.Layer != "repository" || i.Source != RepositoryFile) {
						t.Fatalf("reported at %s:%d (%s), want %s:%d (repository)", i.Source, i.Line, i.Layer, RepositoryFile, tc.line)
					}
					return
				}
			}
			t.Fatalf("want %s: %q; got\n%v", tc.path, tc.text, e.Issues.Err())
		})
	}
}

// Issues read top to bottom: errors before warnings, in line order.
func TestIssuesAreSorted(t *testing.T) {
	file := "version: 1\nroles:\n  reviewer:\n    instructions: {agent: ghost}\nrules:\n  - {on: gate.passed, do: enter_next_phase}\n  - {on: task.reported, do: {start: nobody}}\n"
	e := repo(t, file, Options{Files: fstest.MapFS{}})
	var got []string
	for _, i := range e.Issues {
		got = append(got, fmt.Sprintf("%s %s:%d", i.Severity, i.Source, i.Line))
	}
	want := "error .lazybox/process.yaml:7 warning .lazybox/process.yaml:4 warning preset solo-pair:24"
	if strings.Join(got, " ") != want {
		t.Fatalf("got  %s\nwant %s", strings.Join(got, " "), want)
	}
}

// A trigger cycle that nothing stops is an error; a count limit, a fix-task
// retry budget, an agent budget or a human gate stops one.
func TestTriggerCycles(t *testing.T) {
	loop := "version: 1\ngates:\n  again: {kind: rule, when: \"true\"}\nrules:\n  - {on: gate.passed, do: enter_next_phase}\n  - {on: gate.passed, do: {request_gate: again}%s}\n"
	e := repo(t, strings.Replace(loop, "%s", "", 1), Options{})
	if !hasIssue(e.Issues, "rules[1]", "trigger cycle with no limit: gate.passed → [rules[1]") {
		t.Fatalf("unbounded rule cycle accepted: %v", e.Issues)
	}
	if e = repo(t, strings.Replace(loop, "%s", ", limit: 3", 1), Options{}); !e.Valid() {
		t.Fatalf("a count limit should bound the cycle: %v", e.Issues.Err())
	}
	// Through a human gate the cycle waits for a person.
	human := "version: 1\nrules:\n  - {on: gate.passed, do: enter_next_phase}\n  - {on: gate.passed, do: {request_gate: reviewed}}\n"
	if e = repo(t, human, Options{}); !e.Valid() {
		t.Fatalf("a human gate breaks the cycle: %v", e.Issues.Err())
	}
	// A rule that sends work back to an agent is bounded by the agent's
	// budget (solo-pair's worker has 80 turns), else the mission's.
	agents := "version: 1\nrules:\n  - {on: gate.passed, do: enter_next_phase}\n  - {on: task.reported, do: {send: worker, text: again}}\n"
	if e = repo(t, agents, Options{}); !e.Valid() {
		t.Fatalf("an agent loop under a mission budget is bounded: %v", e.Issues.Err())
	}
	g := e.Process.graph()
	for _, edge := range g.edges {
		if edge.via == "rules[1]" && edge.to == "task.reported" && edge.bound != "roles.worker.budget" {
			t.Fatalf("worker's report is bounded by %q, want roles.worker.budget", edge.bound)
		}
		if edge.via == "rules[1]" && edge.to == "task.question" && edge.bound == "" {
			t.Fatalf("an agent edge is unbounded: %+v", edge)
		}
	}
	// Fix tasks are charged to the task's retries.
	fix := "version: 1\nrules:\n  - {on: gate.passed, do: enter_next_phase}\n  - {on: task.created, do: {create_task: fix}}\n"
	if e = repo(t, fix, Options{}); !e.Valid() {
		t.Fatalf("fix tasks are bounded by retries: %v", e.Issues.Err())
	}
	if e = repo(t, strings.Replace(fix, "create_task: fix", "create_task: chore", 1), Options{}); !hasIssue(e.Issues, "rules[1]", "trigger cycle") {
		t.Fatalf("a task creating tasks forever was accepted: %v", e.Issues)
	}
}

// Instruction files and agent definitions are looked up in the checkout when
// one is given; a missing one is a warning (the role still runs).
func TestInstructionFilesAreCheckedInTheCheckout(t *testing.T) {
	files := fstest.MapFS{
		".lazybox/roles/worker.md":       {Data: []byte("build it")},
		".github/agents/critic.agent.md": {Data: []byte("review it")},
	}
	file := "version: 1\nroles:\n  reviewer:\n    instructions: {agent: critic}\n"
	e := repo(t, file, Options{Files: files})
	if !e.Valid() {
		t.Fatal(e.Issues.Err())
	}
	for _, i := range e.Issues {
		if i.Path == "roles.worker.instructions.file" || i.Path == "roles.reviewer.instructions.agent" {
			t.Fatalf("found file reported missing: %s", i)
		}
	}
	e = repo(t, "version: 1\nroles:\n  reviewer:\n    instructions: {agent: ghost}\n", Options{Files: fstest.MapFS{}})
	var warned []string
	for _, i := range e.Issues {
		if i.Severity == Warning {
			warned = append(warned, i.Path)
		}
	}
	if !e.Valid() || strings.Join(warned, " ") != "roles.reviewer.instructions.agent roles.worker.instructions.file" {
		t.Fatalf("warnings %v valid %v", warned, e.Valid())
	}
}
