package process

// The process file, as Go types. These types are the single source of truth:
// the JSON Schema (schema.go), the structural check every file passes before
// it is decoded (check.go) and the editor completion all come from them and
// their tags:
//
//	yaml, json   the key (always the same)
//	doc          the schema description
//	enum         allowed values, separated by |
//	min, max     numeric bounds
//	required     the key must be present (only in list items, which replace
//	             whole; mapping entries such as roles and gates may be partial
//	             overlays, so their completeness is checked after layering)
//	merge        "replace": an overlay replaces the whole value instead of
//	             merging field by field (one-of values such as Instructions)
//
// A zero value means "not set" for layering: an overlay changes only what it
// sets. That is why numbers that are meaningless at zero (a budget of 0 USD,
// autonomy 0) are plain values, and booleans that a layer must be able to turn
// off are pointers.

// Process is one process file: a built-in preset, the user's file, or a
// repository's `.lazybox/process.yaml`, and also the effective process the
// layers resolve to.
type Process struct {
	Schema        string         `yaml:"$schema,omitempty" json:"$schema,omitempty" doc:"JSON Schema of this file, for editors. Ignored by Lazybox."`
	Version       int            `yaml:"version" json:"version" required:"true" min:"1" max:"1" doc:"Process file format version."`
	Name          string         `yaml:"name,omitempty" json:"name,omitempty" doc:"Name shown in Settings and in a pinned mission."`
	Description   string         `yaml:"description,omitempty" json:"description,omitempty" doc:"What this process is for."`
	Extends       string         `yaml:"extends,omitempty" json:"extends,omitempty" doc:"Built-in preset this file builds on (solo-pair, reviewed-team, factory). The repository's choice wins over the user's; with neither, solo-pair, or reviewed-team once a mission has more than one task."`
	Autonomy      int            `yaml:"autonomy,omitempty" json:"autonomy,omitempty" min:"1" max:"5" doc:"Autonomy level: 3 you review every PR, 4 you approve the contract and accept the result, 5 merges are automatic (needs container isolation)."`
	Requires      *Requires      `yaml:"requires,omitempty" json:"requires,omitempty" doc:"What the Runner must provide for this process to run."`
	Budgets       *Budgets       `yaml:"budgets,omitempty" json:"budgets,omitempty" doc:"Spending limits per mission and per task."`
	WIP           *WIP           `yaml:"wip,omitempty" json:"wip,omitempty" doc:"Work in progress limits."`
	Holdout       *Holdout       `yaml:"holdout,omitempty" json:"holdout,omitempty" doc:"Holdout scenarios: stored outside every worktree, readable only by the listed roles."`
	Phases        []Phase        `yaml:"phases,omitempty" json:"phases,omitempty" doc:"Mission phases in order. Each ends when its exit gate passes."`
	Gates         Map[*Gate]     `yaml:"gates,omitempty" json:"gates,omitempty" doc:"Gates by id. Phases exit through them; rules request them."`
	Roles         Map[*Role]     `yaml:"roles,omitempty" json:"roles,omitempty" doc:"Roles by name. Code knows only the scopes a role may hold, never role names. In an overlay, null removes a role."`
	Rules         []Rule         `yaml:"rules,omitempty" json:"rules,omitempty" doc:"Deterministic reactions to events, in order. Rules handle every mechanical move; roles handle judgment."`
	Integration   *Integration   `yaml:"integration,omitempty" json:"integration,omitempty" doc:"How verified work lands."`
	Notifications []Notification `yaml:"notifications,omitempty" json:"notifications,omitempty" doc:"Which events reach a person, and where."`
	Snippets      *Snippets      `yaml:"snippets,omitempty" json:"snippets,omitempty" doc:"Snippets suggested per phase."`
	Ceiling       *Ceiling       `yaml:"ceiling,omitempty" json:"ceiling,omitempty" doc:"User file only: the most any repository process may ask for."`
}

// Requires names what the Runner must provide.
type Requires struct {
	Isolation string `yaml:"isolation,omitempty" json:"isolation,omitempty" enum:"none|container" doc:"Isolation builders need. Autonomy 5 requires container."`
}

// Budgets bound spending per mission and per task.
type Budgets struct {
	Mission     *Budget `yaml:"mission,omitempty" json:"mission,omitempty" doc:"Limit for the whole mission."`
	Task        *Budget `yaml:"task,omitempty" json:"task,omitempty" doc:"Limit per task. retries also bounds fix tasks created for it."`
	OnExhausted string  `yaml:"on_exhausted,omitempty" json:"on_exhausted,omitempty" enum:"stop|escalate|warn" doc:"What happens when a budget is spent."`
}

// Budget is a set of limits; unset fields are unlimited unless a ceiling
// bounds them.
type Budget struct {
	USD     float64  `yaml:"usd,omitempty" json:"usd,omitempty" min:"0" doc:"Cost in US dollars."`
	Wall    Duration `yaml:"wall,omitempty" json:"wall,omitempty" doc:"Wall-clock time, such as 10h or 30m."`
	Turns   int      `yaml:"turns,omitempty" json:"turns,omitempty" min:"1" doc:"Agent turns."`
	Retries int      `yaml:"retries,omitempty" json:"retries,omitempty" min:"1" doc:"Further attempts (and fix tasks) after the first."`
}

// WIP limits work in progress.
type WIP struct {
	Workers          int    `yaml:"workers,omitempty" json:"workers,omitempty" min:"1" doc:"Live worker sessions in the mission."`
	WritersPerRepo   int    `yaml:"writers_per_repo,omitempty" json:"writers_per_repo,omitempty" min:"1" doc:"Tasks writing to one repository at once."`
	OverlappingPaths string `yaml:"overlapping_paths,omitempty" json:"overlapping_paths,omitempty" enum:"serial|parallel" doc:"Whether tasks touching the same paths run one after another."`
}

// Holdout keeps scenarios away from builders.
type Holdout struct {
	Store   string   `yaml:"store,omitempty" json:"store,omitempty" enum:"runner" doc:"Where scenarios live; runner keeps them in Runner state, outside every worktree."`
	Readers []string `yaml:"readers,omitempty" json:"readers,omitempty" doc:"Roles that may hold holdout.read."`
}

// Phase is one step of a mission.
type Phase struct {
	ID   string `yaml:"id" json:"id" required:"true" doc:"Phase id."`
	Exit string `yaml:"exit" json:"exit" required:"true" doc:"Gate that ends the phase."`
}

// Gate is a checkpoint: a person, a rule, a check or an agent passes it.
type Gate struct {
	Kind string   `yaml:"kind,omitempty" json:"kind,omitempty" enum:"human|rule|check|agent" doc:"Who passes the gate. Only the owner passes a human gate; no role can be given that."`
	When string   `yaml:"when,omitempty" json:"when,omitempty" doc:"CEL condition over mission, task and pr that passes the gate (rule, check and agent gates)."`
	Show []string `yaml:"show,omitempty" json:"show,omitempty" enum:"brief|assertions|task_graph|budget|diff|diffstat|findings|evidence|contract_table|cost|scenarios" doc:"What a person sees when asked to pass it."`
	Role string   `yaml:"role,omitempty" json:"role,omitempty" doc:"Agent gates: the role that judges it."`
}

// Role is a named binding: who does a kind of work, with which provider,
// permission and Lazybox tool scopes.
type Role struct {
	Provider     StringList    `yaml:"provider,omitempty" json:"provider,omitempty" doc:"claude, codex, a list (round-robin across configured accounts), or other_than_<role> (the provider that role is not using)."`
	Model        string        `yaml:"model,omitempty" json:"model,omitempty" doc:"default, strongest, small, or a provider model id."`
	Effort       string        `yaml:"effort,omitempty" json:"effort,omitempty" enum:"low|medium|high|xhigh|max" doc:"Reasoning effort."`
	Permission   string        `yaml:"permission,omitempty" json:"permission,omitempty" enum:"read_only|ask|yolo" doc:"Permission mode. yolo applies only inside the role's own task worktree."`
	Scopes       []string      `yaml:"scopes,omitempty" json:"scopes,omitempty" doc:"Lazybox tools the role may call, such as task.report, verdict.record or session.spawn:worker."`
	Context      []string      `yaml:"context,omitempty" json:"context,omitempty" enum:"brief|task_brief|decisions|board|diff|assertions|assertions_claimed|findings|evidence|scenarios" doc:"What the role is handed when it starts."`
	Instructions *Instructions `yaml:"instructions,omitempty" json:"instructions,omitempty" merge:"replace" doc:"The role's instructions."`
	Budget       *Budget       `yaml:"budget,omitempty" json:"budget,omitempty" doc:"Limits per instance of the role."`
	Triggers     []Trigger     `yaml:"triggers,omitempty" json:"triggers,omitempty" doc:"Events that wake the role."`
	Concurrency  int           `yaml:"concurrency,omitempty" json:"concurrency,omitempty" min:"1" doc:"Live instances of the role."`
	WIP          int           `yaml:"wip,omitempty" json:"wip,omitempty" min:"1" doc:"Tasks one instance holds at once."`
}

// Instructions name exactly one source of a role's instructions.
type Instructions struct {
	File    string `yaml:"file,omitempty" json:"file,omitempty" doc:"A file in the repository, relative to its root."`
	Snippet string `yaml:"snippet,omitempty" json:"snippet,omitempty" doc:"A snippet key."`
	Text    string `yaml:"text,omitempty" json:"text,omitempty" doc:"Inline text."`
	Agent   string `yaml:"agent,omitempty" json:"agent,omitempty" doc:"An existing agent definition by name: .claude/agents/<name>.md, .github/agents/<name>.md (or .agent.md), or .factory/droids/<name>.md."`
}

// Trigger wakes a role on an event.
type Trigger struct {
	On    StringList `yaml:"on" json:"on" required:"true" doc:"Event types, or the synthetic timer and stall."`
	If    string     `yaml:"if,omitempty" json:"if,omitempty" doc:"CEL condition over the event's variables."`
	Mode  string     `yaml:"mode,omitempty" json:"mode,omitempty" enum:"auto|manual" doc:"manual: a person starts the role from the inbox."`
	Every Duration   `yaml:"every,omitempty" json:"every,omitempty" doc:"timer: the period."`
	After Duration   `yaml:"after,omitempty" json:"after,omitempty" doc:"stall: how long a live task may go without an event."`
}

// Rule is a deterministic reaction: on an event, if a condition holds, do one
// action.
type Rule struct {
	On    StringList `yaml:"on" json:"on" required:"true" doc:"Event types."`
	If    string     `yaml:"if,omitempty" json:"if,omitempty" doc:"CEL condition over the event's variables."`
	Do    Action     `yaml:"do" json:"do" required:"true" doc:"The action."`
	Limit int        `yaml:"limit,omitempty" json:"limit,omitempty" min:"1" doc:"Fire at most this many times per subject (a count limit that bounds a trigger cycle)."`
}

// Action is one verb. In YAML it is a mapping with one verb key (plus that
// verb's arguments), or the bare string enter_next_phase.
type Action struct {
	Start            string   `yaml:"start,omitempty" json:"start,omitempty" doc:"Start a new instance of this role on the event's task."`
	Wake             string   `yaml:"wake,omitempty" json:"wake,omitempty" doc:"Wake this role with the event."`
	Send             string   `yaml:"send,omitempty" json:"send,omitempty" doc:"Send a message to this role's session on the task (with snippet or text)."`
	Snippet          string   `yaml:"snippet,omitempty" json:"snippet,omitempty" doc:"send: the snippet to send."`
	Text             string   `yaml:"text,omitempty" json:"text,omitempty" doc:"send: the text to send."`
	RequestGate      string   `yaml:"request_gate,omitempty" json:"request_gate,omitempty" doc:"Request this gate."`
	EnterPhase       string   `yaml:"enter_phase,omitempty" json:"enter_phase,omitempty" doc:"Enter this phase (refused until the current phase's exit gate has passed)."`
	EnterNextPhase   bool     `yaml:"enter_next_phase,omitempty" json:"enter_next_phase,omitempty" doc:"Enter the next phase (refused until the current phase's exit gate has passed). Also written as the bare string enter_next_phase."`
	CreateTask       string   `yaml:"create_task,omitempty" json:"create_task,omitempty" doc:"Create a task of this kind (fix: charged to the parent task's retries)."`
	Assign           string   `yaml:"assign,omitempty" json:"assign,omitempty" doc:"create_task: the role that takes it."`
	Notify           []string `yaml:"notify,omitempty" json:"notify,omitempty" enum:"inbox|desktop" doc:"Notify a person."`
	Escalate         string   `yaml:"escalate,omitempty" json:"escalate,omitempty" doc:"Raise an escalation with this reason."`
	Stop             string   `yaml:"stop,omitempty" json:"stop,omitempty" doc:"Stop this role's session on the task."`
	Exec             string   `yaml:"exec,omitempty" json:"exec,omitempty" doc:"Run this repository script through lazybox run, with the event in LAZYBOX_EVENT; its output is evidence."`
	IntegrationQueue string   `yaml:"integration.queue,omitempty" json:"integration.queue,omitempty" enum:"task" doc:"Queue the event's task for integration."`
}

// Integration says how verified work lands.
type Integration struct {
	Strategy       string  `yaml:"strategy,omitempty" json:"strategy,omitempty" enum:"queue|pr_per_task|manual" doc:"queue: a merge queue per repository; pr_per_task: one PR each; manual: a person lands it."`
	Order          string  `yaml:"order,omitempty" json:"order,omitempty" enum:"dependency|fifo" doc:"dependency: follow merge_after edges."`
	Method         string  `yaml:"method,omitempty" json:"method,omitempty" enum:"squash|merge|rebase" doc:"Merge method."`
	AutoMerge      *bool   `yaml:"auto_merge,omitempty" json:"auto_merge,omitempty" doc:"Merge without a person once checks and gates pass (autonomy 5)."`
	AfterEachMerge *Action `yaml:"after_each_merge,omitempty" json:"after_each_merge,omitempty" merge:"replace" doc:"Action after each merge, usually exec of an integration check."`
	OnFailure      string  `yaml:"on_failure,omitempty" json:"on_failure,omitempty" enum:"bisect|stop|escalate" doc:"What to do when a post-merge check fails."`
}

// Notification routes events to a person.
type Notification struct {
	On       StringList `yaml:"on" json:"on" required:"true" doc:"Event types."`
	To       []string   `yaml:"to" json:"to" required:"true" enum:"inbox|desktop" doc:"Where they go."`
	Priority string     `yaml:"priority,omitempty" json:"priority,omitempty" enum:"low|normal|high" doc:"Priority."`
	Digest   string     `yaml:"digest,omitempty" json:"digest,omitempty" enum:"hourly|daily" doc:"Group into a digest instead of one notification each."`
}

// Snippets suggests snippets per phase.
type Snippets struct {
	Suggest map[string][]string `yaml:"suggest,omitempty" json:"suggest,omitempty" doc:"Phase id to snippet keys."`
}

// Ceiling is the most a repository process may ask for. It is set in the
// user's file only; the effective process must fit it whatever its layers say.
type Ceiling struct {
	Autonomy   int      `yaml:"autonomy,omitempty" json:"autonomy,omitempty" min:"1" max:"5" doc:"Highest autonomy level."`
	Permission string   `yaml:"permission,omitempty" json:"permission,omitempty" enum:"read_only|ask|yolo" doc:"Highest permission any role may hold."`
	Providers  []string `yaml:"providers,omitempty" json:"providers,omitempty" enum:"claude|codex" doc:"Providers roles may use."`
	Budgets    *Budgets `yaml:"budgets,omitempty" json:"budgets,omitempty" doc:"Highest budgets. An unset budget in the process takes the ceiling's value."`
}
