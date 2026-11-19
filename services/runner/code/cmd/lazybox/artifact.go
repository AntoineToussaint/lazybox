package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"slices"
	"strings"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// `lazybox artifact` publishes and reads the workspace's artifacts: plans,
// reports and specs an agent writes for the person, kept by the Runner as
// versioned pages (ARTIFACTS.md). An agent's credential reaches only its
// own workspace's artifacts, and retires only the ones agents published.

// artifactAPI is the part of the Runner API the verb uses.
type artifactAPI interface {
	ListArtifacts(context.Context, *connect.Request[pb.ListArtifactsRequest]) (*connect.Response[pb.ListArtifactsResponse], error)
	GetArtifact(context.Context, *connect.Request[pb.GetArtifactRequest]) (*connect.Response[pb.Artifact], error)
	PutArtifact(context.Context, *connect.Request[pb.PutArtifactRequest]) (*connect.Response[pb.Artifact], error)
	RetireArtifact(context.Context, *connect.Request[pb.RetireArtifactRequest]) (*connect.Response[pb.Artifact], error)
}

const artifactUsage = `Usage:
  lazybox artifact put <file|-> --title T [--id ID] [--kind plan|report|spec|notes|other] [--format markdown|html] [--supersedes ID[,ID…]]
  lazybox artifact retire ID [--superseded-by ID] [--reason "…"]
  lazybox artifact get ID [--version N]
  lazybox artifact list [--all]`

// restorable ends what retire and put --supersedes print: nothing was deleted.
const restorable = "Retired artifacts keep every version; the person can restore them in Lazybox → Artifacts."

// retiredLine is the line the conversation's card reads for each artifact
// retired (packages/surfaces/src/tool-lines.ts, retiredArtifacts).
func retiredLine(a *pb.Artifact, by *pb.Artifact) string {
	line := fmt.Sprintf("Retired “%s” (artifact %s)", a.Title, a.Id)
	if by != nil {
		line += fmt.Sprintf(", superseded by “%s” (artifact %s)", by.Title, by.Id)
	}
	return line + "."
}

// splitIDs reads `--supersedes A,B` (spaces allowed), each once.
func splitIDs(value string) []string {
	var ids []string
	for _, id := range strings.FieldsFunc(value, func(r rune) bool { return r == ',' || r == ' ' }) {
		if !slices.Contains(ids, id) {
			ids = append(ids, id)
		}
	}
	return ids
}

const noWorkspace = "artifacts belong to a workspace; run this from a Lazybox agent working in one, or pass --collection"

func artifactVerb(ctx context.Context, endpointPath string, args []string, stdin io.Reader, stdout, stderr io.Writer) error {
	address, err := readEndpoint(endpointPath)
	if err != nil {
		return err
	}
	client := genconnect.NewRunnerServiceClient(newHTTPClient(address.Token), address.URL)
	return artifactCommand(ctx, client, os.Getenv("LAZYBOX_V2_COLLECTION_ID"), args, stdin, stdout, stderr)
}

// parseInterleaved parses flags wherever they appear among the positional
// arguments (`put plan.md --title T` as well as `put --title T plan.md`).
func parseInterleaved(flags *flag.FlagSet, args []string) ([]string, error) {
	var positional []string
	for {
		if err := flags.Parse(args); err != nil {
			return nil, err
		}
		rest := flags.Args()
		if len(rest) == 0 {
			return positional, nil
		}
		positional = append(positional, rest[0])
		args = rest[1:]
	}
}

func artifactCommand(ctx context.Context, client artifactAPI, workspace string, args []string, stdin io.Reader, stdout, stderr io.Writer) error {
	if len(args) == 0 {
		fmt.Fprintln(stderr, artifactUsage)
		return errors.New("lazybox artifact needs put, retire, get or list")
	}
	verb := args[0]
	flags := flag.NewFlagSet("artifact "+verb, flag.ContinueOnError)
	flags.SetOutput(stderr)
	flags.Usage = func() { fmt.Fprintln(stderr, artifactUsage); flags.PrintDefaults() }
	collection := flags.String("collection", workspace, "workspace whose artifacts to use (set for agents and workspace terminals)")
	title := flags.String("title", "", "put: the artifact's title (required to create one)")
	id := flags.String("id", "", "put: publish a new version of this artifact instead of creating one")
	format := flags.String("format", "", "put: markdown or html (set at creation)")
	kind := flags.String("kind", "", "put: plan, report, spec, notes or other")
	version := flags.Uint("version", 0, "get: this version instead of the latest")
	supersedes := flags.String("supersedes", "", "put: retire these artifacts (comma-separated IDs) as superseded by the one published")
	supersededBy := flags.String("superseded-by", "", "retire: the artifact that replaces it")
	reason := flags.String("reason", "", "retire: one line on why")
	all := flags.Bool("all", false, "list: include retired artifacts")
	positional, err := parseInterleaved(flags, args[1:])
	if err != nil {
		return err
	}
	if *collection == "" {
		return errors.New(noWorkspace)
	}
	switch verb {
	case "put":
		if len(positional) != 1 {
			return errors.New("usage: lazybox artifact put <file|-> --title T [--id ID] [--kind plan]")
		}
		if *id == "" && strings.TrimSpace(*title) == "" {
			return errors.New("--title is required to create an artifact (or pass --id ID to publish a new version)")
		}
		var data []byte
		if positional[0] == "-" {
			data, err = io.ReadAll(io.LimitReader(stdin, 1<<20+1))
		} else {
			data, err = os.ReadFile(positional[0])
		}
		if err != nil {
			return err
		}
		previous := uint32(0)
		if *id != "" {
			current, err := client.GetArtifact(ctx, connect.NewRequest(&pb.GetArtifactRequest{CollectionId: *collection, ArtifactId: *id}))
			if err != nil {
				return err
			}
			previous = current.Msg.ContentVersion
		}
		// The artifacts it supersedes, read first for their titles: an unknown ID stops here.
		var replaced []*pb.Artifact
		for _, old := range splitIDs(*supersedes) {
			current, err := client.GetArtifact(ctx, connect.NewRequest(&pb.GetArtifactRequest{CollectionId: *collection, ArtifactId: old}))
			if err != nil {
				return fmt.Errorf("--supersedes %s: %w", old, err)
			}
			replaced = append(replaced, current.Msg)
		}
		request := &pb.PutArtifactRequest{CollectionId: *collection, ArtifactId: *id, Title: *title, Kind: *kind, Format: *format, Content: string(data)}
		for _, old := range replaced {
			request.Supersedes = append(request.Supersedes, old.Id)
		}
		r, err := client.PutArtifact(ctx, connect.NewRequest(request))
		if err != nil {
			return err
		}
		a := r.Msg
		if previous != 0 && a.ContentVersion == previous {
			_, err = fmt.Fprintf(stdout, "Artifact %s “%s” unchanged at version %d (same content).\n", a.Id, a.Title, a.ContentVersion)
		} else {
			_, err = fmt.Fprintf(stdout, "Published “%s” as artifact %s, version %d (%s). The user sees it in Lazybox → Artifacts. Publish a new version with --id %s.\n", a.Title, a.Id, a.ContentVersion, a.Kind, a.Id)
		}
		if err != nil || len(replaced) == 0 {
			return err
		}
		for _, old := range replaced {
			if _, err = fmt.Fprintln(stdout, retiredLine(old, a)); err != nil {
				return err
			}
		}
		_, err = fmt.Fprintln(stdout, restorable)
		return err
	case "retire":
		if len(positional) != 1 {
			return errors.New(`usage: lazybox artifact retire ID [--superseded-by ID] [--reason "…"]`)
		}
		var by *pb.Artifact
		if *supersededBy != "" {
			current, err := client.GetArtifact(ctx, connect.NewRequest(&pb.GetArtifactRequest{CollectionId: *collection, ArtifactId: *supersededBy}))
			if err != nil {
				return fmt.Errorf("--superseded-by %s: %w", *supersededBy, err)
			}
			by = current.Msg
		}
		r, err := client.RetireArtifact(ctx, connect.NewRequest(&pb.RetireArtifactRequest{CollectionId: *collection, ArtifactId: positional[0], SupersededBy: *supersededBy, Reason: *reason}))
		if err != nil {
			return err
		}
		_, err = fmt.Fprintf(stdout, "%s %s\n", retiredLine(r.Msg, by), restorable)
		return err
	case "get":
		if len(positional) != 1 {
			return errors.New("usage: lazybox artifact get ID [--version N]")
		}
		r, err := client.GetArtifact(ctx, connect.NewRequest(&pb.GetArtifactRequest{CollectionId: *collection, ArtifactId: positional[0], Version: uint32(*version)}))
		if err != nil {
			return err
		}
		content := r.Msg.Content
		if !strings.HasSuffix(content, "\n") {
			content += "\n"
		}
		_, err = io.WriteString(stdout, content)
		return err
	case "list":
		if len(positional) != 0 {
			return errors.New("usage: lazybox artifact list [--all]")
		}
		r, err := client.ListArtifacts(ctx, connect.NewRequest(&pb.ListArtifactsRequest{CollectionId: *collection, IncludeRetired: *all}))
		if err != nil {
			return err
		}
		if len(r.Msg.Artifacts) == 0 {
			_, err = fmt.Fprintln(stdout, "No artifacts in this workspace yet.")
			return err
		}
		for _, a := range r.Msg.Artifacts {
			latest := uint32(0)
			if n := len(a.Versions); n > 0 {
				latest = a.Versions[n-1].Number
			}
			retired := ""
			if r := a.Retired; r != nil {
				retired = "\tretired"
				if r.SupersededBy != "" {
					retired += " (superseded by " + r.SupersededBy + ")"
				}
			}
			if _, err = fmt.Fprintf(stdout, "%s\t%s\tv%d\t%s%s\n", a.Id, a.Kind, latest, a.Title, retired); err != nil {
				return err
			}
		}
		return nil
	}
	fmt.Fprintln(stderr, artifactUsage)
	return fmt.Errorf("unknown artifact command %q", verb)
}
