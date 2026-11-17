package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/process"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

const processUsage = `Usage: lazybox process <validate|show|explain EVENT|events|schema> [options]

Checks and explains the orchestration process (ORCHESTRATION.md §4).
Without --workspace it reads files: the repository's .lazybox/process.yaml
(or --file), over --user and the built-in presets. With --workspace it asks
the Runner for that workspace's effective process (owner credential).

  validate        schema and semantic checks; exits 1 on any error
  show            the effective process as YAML (--json for JSON)
  explain EVENT   what fires on EVENT, in order, with each role's scopes
  events          the event types a process may name, with their CEL variables
  schema          the JSON Schema of the process file
`

// processVerb runs `lazybox process …`. dial connects to the Runner, only
// when --workspace asks for one.
func processVerb(ctx context.Context, args []string, stdout, stderr io.Writer, dial func() (genconnect.RunnerServiceClient, error)) error {
	if len(args) == 0 || args[0] == "help" || args[0] == "-h" || args[0] == "--help" {
		fmt.Fprint(stdout, processUsage)
		return nil
	}
	sub := args[0]
	flags := flag.NewFlagSet("process "+sub, flag.ContinueOnError)
	flags.SetOutput(stderr)
	file := flags.String("file", "", "repository process file (default: ./"+process.RepositoryFile+" when it exists)")
	user := flags.String("user", "", "user process file: your defaults and ceiling (default: none, the built-in ceiling)")
	preset := flags.String("preset", "", "preset to use when no file extends one (default: solo-pair, or reviewed-team with --tasks > 1)")
	tasks := flags.Int("tasks", 0, "tasks in the mission, which selects the default preset")
	isolation := flags.String("isolation", "none", "isolation the Runner provides for builders: none or container")
	workspace := flags.String("workspace", "", "ask the Runner for this workspace's effective process instead of reading files")
	repository := flags.String("repository", "", "with --workspace: the repository whose process file applies")
	asJSON := flags.Bool("json", false, "show: print JSON instead of YAML")
	flags.Usage = func() { fmt.Fprint(stderr, processUsage); flags.PrintDefaults() }
	var event string
	rest := args[1:]
	if sub == "explain" {
		if len(rest) == 0 || strings.HasPrefix(rest[0], "-") {
			return errors.New("explain needs an event type, such as `lazybox process explain task.reported`")
		}
		event, rest = rest[0], rest[1:]
	}
	if err := flags.Parse(rest); err != nil {
		return err
	}
	switch sub {
	case "schema":
		_, err := stdout.Write(process.Schema())
		return err
	case "events":
		for _, e := range process.Events() {
			vars := append([]string{"event", "mission"}, e.Vars...)
			kind := ""
			if e.Synthetic {
				kind = " (synthetic, not stored)"
			}
			fmt.Fprintf(stdout, "%-24s %-13s %s%s\n", e.Name, e.Family, strings.Join(vars, ", "), kind)
		}
		return nil
	case "validate", "show", "explain":
	default:
		return fmt.Errorf("unknown process command %q\n%s", sub, processUsage)
	}

	if *workspace != "" {
		client, err := dial()
		if err != nil {
			return err
		}
		return remoteProcess(ctx, client, sub, event, &pb.OrchestrationProcessRequest{CollectionId: *workspace, Repository: *repository, Tasks: uint32(max(*tasks, 0))}, *asJSON, stdout)
	}

	options := process.Options{Preset: *preset, Tasks: *tasks, Isolation: *isolation}
	var userFile, repoFile *process.File
	if *user != "" {
		data, err := os.ReadFile(*user)
		if err != nil {
			return err
		}
		userFile = process.Parse(process.LayerUser, *user, data)
	}
	path := *file
	if path == "" {
		if _, err := os.Stat(process.RepositoryFile); err == nil {
			path = process.RepositoryFile
		}
	}
	if path != "" {
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		repoFile = process.Parse(process.LayerRepository, path, data)
		if abs, err := filepath.Abs(path); err == nil && strings.HasSuffix(filepath.ToSlash(abs), "/"+process.RepositoryFile) {
			options.Files = os.DirFS(filepath.Dir(filepath.Dir(abs)))
		}
	}
	e := process.Resolve(userFile, repoFile, options)
	switch sub {
	case "validate":
		return printValidation(stdout, e)
	case "show":
		if e.Process == nil {
			return printValidation(stdout, e)
		}
		return showProcess(stdout, e.Process, *asJSON)
	default:
		if e.Process == nil {
			return printValidation(stdout, e)
		}
		reactions, err := e.Process.Reactions(event)
		if err != nil {
			return err
		}
		fmt.Fprint(stdout, process.FormatReactions(event, reactions))
		return nil
	}
}

func layerLine(layers []process.LayerInfo) string {
	parts := make([]string, len(layers))
	for i, l := range layers {
		parts[i] = string(l.Kind) + " " + l.Source
	}
	return strings.Join(parts, " ← ")
}

func printValidation(w io.Writer, e *process.Effective) error {
	if len(e.Layers) > 0 {
		fmt.Fprintln(w, layerLine(e.Layers))
	}
	for _, i := range e.Issues {
		fmt.Fprintln(w, i.String())
	}
	errs := len(e.Issues.Errors())
	if e.Valid() {
		fmt.Fprintf(w, "valid · %s\n", e.Digest)
		return nil
	}
	return fmt.Errorf("the process is invalid: %d error(s)", max(errs, 1))
}

func showProcess(w io.Writer, p *process.Process, asJSON bool) error {
	if asJSON {
		data, err := json.MarshalIndent(p, "", "  ")
		if err != nil {
			return err
		}
		_, err = fmt.Fprintln(w, string(data))
		return err
	}
	data, err := p.YAML()
	if err != nil {
		return err
	}
	_, err = w.Write(data)
	return err
}

// remoteProcess is the same three commands against the Runner's view of a
// workspace: the user's file there, the workspace's repository file.
func remoteProcess(ctx context.Context, client genconnect.RunnerServiceClient, sub, event string, request *pb.OrchestrationProcessRequest, asJSON bool, w io.Writer) error {
	if sub == "explain" {
		r, err := client.ExplainOrchestrationProcess(ctx, connect.NewRequest(&pb.ExplainOrchestrationProcessRequest{Process: request, EventType: event}))
		if err != nil {
			return err
		}
		_, err = fmt.Fprint(w, r.Msg.Text)
		return err
	}
	r, err := client.GetOrchestrationProcess(ctx, connect.NewRequest(request))
	if err != nil {
		return err
	}
	p := r.Msg
	if p.Notice != "" {
		fmt.Fprintln(w, p.Notice)
	}
	if sub == "show" && p.Yaml != "" {
		if asJSON {
			_, err = fmt.Fprintln(w, p.Json)
		} else {
			_, err = fmt.Fprint(w, p.Yaml)
		}
		return err
	}
	parts := make([]string, len(p.Layers))
	for i, l := range p.Layers {
		parts[i] = l.Kind + " " + l.Source
	}
	fmt.Fprintln(w, strings.Join(parts, " ← "))
	errs := 0
	for _, i := range p.Issues {
		issue := process.Issue{Severity: process.Severity(i.Severity), Path: i.Path, Message: i.Message, Source: i.Source, Line: int(i.Line), Column: int(i.Column)}
		fmt.Fprintln(w, issue.String())
		if issue.Severity == process.Error {
			errs++
		}
	}
	if p.Valid {
		fmt.Fprintf(w, "valid · %s\n", p.Digest)
		return nil
	}
	return fmt.Errorf("the process is invalid: %d error(s)", max(errs, 1))
}
