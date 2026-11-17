// Package workspaceruntime reads a repository's committed runtime
// configuration (.lazybox/workspace.yaml: setup, archive, actions and files to
// copy), decides whether the person approved exactly that configuration,
// allocates each workspace's port block and copies listed files from a source
// checkout into a new worktree. It runs nothing itself: the Runner starts the
// scripts as managed commands (runner/workspace_runtime.go).
//
// The format is YAML, like Lazybox's own configuration (V1's config.yaml,
// workspace.codefly.yaml): setup and archive scripts are multi-line shell, and
// YAML's block scalars keep them readable and diffable in review. Decoding is
// strict (an unknown key is an error with its line), so a typo never silently
// disables a script. workspace.schema.json is the same contract for editors.
package workspaceruntime
