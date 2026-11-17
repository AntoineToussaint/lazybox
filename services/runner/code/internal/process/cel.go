package process

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"
	"sync"
	"time"

	"cel.dev/cel-go/cel"
	"cel.dev/cel-go/common/types"
)

// Conditions are CEL (https://cel.dev): not Turing-complete, side-effect
// free, and bounded here at every stage, because a repository's process file
// is untrusted input evaluated on every event:
//
//   - source: at most MaxConditionBytes; the parser's recursion depth is capped;
//   - checking: only the event's own variables exist (a rule on task.reported
//     cannot read gate), the result must be a bool, comprehensions (all,
//     exists, map, filter) nest at most two deep;
//   - evaluation: a cost limit stops runaway comprehensions over large lists,
//     and a deadline bounds wall time whatever the input.
const (
	MaxConditionBytes  = 1024
	maxRecursion       = 32
	maxComprehensions  = 2
	conditionCostLimit = 100_000
)

// conditionDeadline bounds one evaluation's wall time (a variable so tests
// can tell the cost limit and the deadline apart on a loaded machine).
var conditionDeadline = 50 * time.Millisecond

// Condition is a compiled CEL condition.
type Condition struct {
	Source string
	vars   []string
	prg    cel.Program
}

// CompileCondition compiles a rule or trigger condition for an event type.
func CompileCondition(expr, event string) (*Condition, error) {
	e, ok := LookupEvent(event)
	if !ok {
		return nil, fmt.Errorf("unknown event type %q", event)
	}
	return compile(expr, e.Vars)
}

// CompileGateCondition compiles a gate's when condition.
func CompileGateCondition(expr string) (*Condition, error) { return compile(expr, gateVars) }

// Vars are the variables a condition may read, sorted: event and mission
// always, plus the event's own.
func (c *Condition) Vars() []string { return slices.Clone(c.vars) }

// Eval evaluates the condition. Each variable is a map[string]any (or nil);
// variables the condition may read but the caller leaves out are bound to
// empty maps, so reading a field of one is an evaluation error, not a silent
// false. The caller treats any error as "did not fire" and records it.
func (c *Condition) Eval(ctx context.Context, vars map[string]any) (bool, error) {
	activation := make(map[string]any, len(c.vars))
	for _, name := range c.vars {
		v, ok := vars[name]
		if !ok || v == nil {
			v = map[string]any{}
		}
		activation[name] = v
	}
	ctx, cancel := context.WithTimeout(ctx, conditionDeadline)
	defer cancel()
	out, _, err := c.prg.ContextEval(ctx, activation)
	if err != nil {
		return false, err
	}
	result, ok := out.(types.Bool)
	if !ok {
		return false, fmt.Errorf("condition returned %s, not a bool", out.Type().TypeName())
	}
	return bool(result), nil
}

var envs sync.Map // sorted vars joined by "," → *cel.Env

func environment(vars []string) (*cel.Env, error) {
	key := strings.Join(vars, ",")
	if env, ok := envs.Load(key); ok {
		return env.(*cel.Env), nil
	}
	options := []cel.EnvOption{
		cel.ParserRecursionLimit(maxRecursion),
		cel.ParserExpressionSizeLimit(MaxConditionBytes),
		cel.ASTValidators(cel.ValidateComprehensionNestingLimit(maxComprehensions)),
	}
	for _, name := range vars {
		options = append(options, cel.Variable(name, cel.MapType(cel.StringType, cel.DynType)))
	}
	env, err := cel.NewEnv(options...)
	if err != nil {
		return nil, err
	}
	envs.Store(key, env)
	return env, nil
}

func compile(expr string, eventVars []string) (*Condition, error) {
	if strings.TrimSpace(expr) == "" {
		return nil, errors.New("empty condition")
	}
	if len(expr) > MaxConditionBytes {
		return nil, fmt.Errorf("condition is %d bytes; the limit is %d", len(expr), MaxConditionBytes)
	}
	vars := append([]string{"event", "mission"}, eventVars...)
	slices.Sort(vars)
	vars = slices.Compact(vars)
	env, err := environment(vars)
	if err != nil {
		return nil, err
	}
	ast, issues := env.Compile(expr)
	if issues != nil && issues.Err() != nil {
		var messages []string
		for _, e := range issues.Errors() {
			messages = append(messages, fmt.Sprintf("column %d: %s", e.Location.Column()+1, e.Message))
		}
		return nil, fmt.Errorf("%s (variables here: %s)", strings.Join(messages, "; "), strings.Join(vars, ", "))
	}
	if t := ast.OutputType(); !t.IsExactType(types.BoolType) && !t.IsExactType(types.DynType) {
		return nil, fmt.Errorf("condition must be true or false, not %s", t)
	}
	prg, err := env.Program(ast, cel.CostLimit(conditionCostLimit), cel.InterruptCheckFrequency(64))
	if err != nil {
		return nil, err
	}
	return &Condition{Source: expr, vars: vars, prg: prg}, nil
}
