// Package process is the orchestration process file (ORCHESTRATION.md §4):
// its Go types, JSON Schema, built-in presets, layering under the user's
// ceiling, validation, CEL conditions and explain. It starts no agent and
// keeps no state; a mission runtime pins an Effective process and asks it
// what to do.
//
// # Layers
//
// A process is resolved from up to three files, lowest first:
//
//  1. a built-in preset (solo-pair, reviewed-team, factory), chosen by the
//     repository's `extends`, else the user's, else DefaultPreset(tasks);
//  2. the user's file (the Runner's process.yaml): defaults, and the ceiling;
//  3. the repository's committed RepositoryFile.
//
// An overlay changes only what it sets: scalars when non-zero, nested
// mappings field by field, lists replace whole, roles and gates merge by
// name (null removes one). The effective process must fit the user's ceiling
// (DefaultCeiling when the user sets none): autonomy, role permissions,
// providers and budgets above it are errors, not silently clamped, and
// unset budgets take the ceiling's value. A repository file cannot set a
// ceiling. Its Digest is what a person approves before a mission first uses
// it; recording that approval is the mission runtime's job.
//
// # API
//
//	f := process.Parse(process.LayerRepository, path, data) // one layer; f.Issues
//	e := process.Resolve(userFile, repoFile, process.Options{Tasks: n})
//	e.Valid(); e.Issues; e.Process; e.Digest            // pin e.Process and e.Digest
//	rs, err := e.Process.Reactions("task.reported")     // rules, triggers, notifications in firing order
//	c, err := process.CompileCondition(rs[0].Condition, "task.reported")
//	ok, err := c.Eval(ctx, map[string]any{"task": taskVars})
//
// Events and Scopes list the catalogs a file may name; LookupEvent gives the
// CEL variables a condition on an event may read. Schema returns the JSON
// Schema generated from the types (committed as schema/process.v1.json).
//
// # Semantics a runtime must honour
//
//   - enter_phase and enter_next_phase are refused until the current phase's
//     exit gate has passed. A rule or check gate passes when its condition
//     holds; human and agent gates are requested.
//   - A rule's limit is a count per subject. A create_task of kind fix is
//     charged to its task's budgets.task.retries. These, agent budgets and
//     human gates are what validation accepts as bounding a trigger cycle.
//   - Condition evaluation errors mean "did not fire"; record them.
//   - A role's permission defaults to read_only.
package process
