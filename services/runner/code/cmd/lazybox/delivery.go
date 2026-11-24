package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"

	"connectrpc.com/connect"
	"google.golang.org/protobuf/encoding/protojson"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

type deliveryAPI interface {
	BindDelivery(context.Context, *connect.Request[pb.BindDeliveryRequest]) (*connect.Response[pb.DeliveryBinding], error)
	GetDeliveryOutcome(context.Context, *connect.Request[pb.GetDeliveryOutcomeRequest]) (*connect.Response[pb.DeliveryOutcome], error)
	PublishDeliveryOutcome(context.Context, *connect.Request[pb.PublishDeliveryOutcomeRequest]) (*connect.Response[pb.Artifact], error)
	RecordReviewCriterion(context.Context, *connect.Request[pb.RecordReviewCriterionRequest]) (*connect.Response[pb.RecordReviewCriterionResponse], error)
}

const deliveryUsage = `Usage:
  lazybox delivery bind <spec-id>@<version>
  lazybox delivery status [--json] [--mission ID --task ID]
  lazybox delivery publish [--id ARTIFACT] [--mission ID --task ID]
  lazybox delivery review <criterion> passed|failed

bind pins a published spec.v2 version to this conversation (start one with
lazybox artifact template delivery-spec). Then its runs record evidence:
  lazybox run --check <criterion> -- <command>   exit 0 records a pass, any other exit a failure
  lazybox run --needs <criterion> -- <command>   a non-zero exit records a failed prerequisite (blocked)
A conversation working a Mission task checks against the plan's approved
contract and needs no bind. Binding another version starts a new attempt.
status is the verdict on the workspace's current files and every candidate
checked; publish keeps it as a versioned report artifact; review records a
contract-bound reviewer's verdict on one criterion.`

func deliveryVerb(ctx context.Context, endpointPath string, args []string, stdout, stderr io.Writer) error {
	address, err := readEndpoint(endpointPath)
	if err != nil {
		return err
	}
	client := genconnect.NewRunnerServiceClient(newHTTPClient(address.Token), address.URL)
	return deliveryCommand(ctx, client, os.Getenv("LAZYBOX_V2_SESSION_ID"), args, stdout, stderr)
}

func deliveryCommand(ctx context.Context, client deliveryAPI, session string, args []string, stdout, stderr io.Writer) error {
	if len(args) == 0 {
		fmt.Fprintln(stderr, deliveryUsage)
		return errors.New("lazybox delivery needs bind, status, publish or review")
	}
	verb := args[0]
	flags := flag.NewFlagSet("delivery "+verb, flag.ContinueOnError)
	flags.SetOutput(stderr)
	flags.Usage = func() { fmt.Fprintln(stderr, deliveryUsage); flags.PrintDefaults() }
	asJSON := flags.Bool("json", false, "status: the outcome as JSON")
	mission := flags.String("mission", "", "status, publish: a Mission's task instead of this conversation (with --task)")
	task := flags.String("task", "", "status, publish: the task of --mission")
	artifact := flags.String("id", "", "publish: a new version of this verdict snapshot")
	positional, err := parseInterleaved(flags, args[1:])
	if err != nil {
		return err
	}
	lookup := func() (string, string, string, error) {
		if (*mission == "") != (*task == "") {
			return "", "", "", errors.New("--mission and --task go together")
		}
		if *mission != "" {
			return "", *mission, *task, nil
		}
		if session == "" {
			return "", "", "", errors.New("run it from a Lazybox agent, or name a task with --mission and --task")
		}
		return session, "", "", nil
	}
	switch verb {
	case "bind":
		if len(positional) != 1 {
			return errors.New("lazybox delivery bind <spec-id>@<version>")
		}
		if session == "" {
			return errors.New("lazybox delivery bind runs from a Lazybox agent: it binds that agent's conversation")
		}
		id, v, ok := strings.Cut(positional[0], "@")
		version, err := strconv.ParseUint(v, 10, 32)
		if !ok || id == "" || err != nil || version == 0 {
			return fmt.Errorf("write the spec as <spec-id>@<version>, for example %s@3", firstNonEmpty(id, "spec-id"))
		}
		r, err := client.BindDelivery(ctx, connect.NewRequest(&pb.BindDeliveryRequest{SessionId: session, ArtifactId: id, Version: uint32(version)}))
		if err != nil {
			return err
		}
		b := r.Msg
		fmt.Fprintf(stdout, "Bound %s to this conversation (attempt %s).\nCheck each criterion with lazybox run --check <id> -- <command>:\n", specName(b.Title, b.ArtifactId, b.Version), b.Attempt)
		for _, c := range b.Criteria {
			fmt.Fprintln(stdout, "- "+c)
		}
		return nil
	case "status":
		s, m, t, err := lookup()
		if err != nil {
			return err
		}
		r, err := client.GetDeliveryOutcome(ctx, connect.NewRequest(&pb.GetDeliveryOutcomeRequest{SessionId: s, MissionId: m, TaskId: t}))
		if err != nil {
			return err
		}
		if *asJSON {
			data, err := protojson.MarshalOptions{Multiline: true, EmitUnpopulated: false}.Marshal(r.Msg)
			if err != nil {
				return err
			}
			_, err = fmt.Fprintln(stdout, string(data))
			return err
		}
		_, err = io.WriteString(stdout, outcomeText(r.Msg))
		return err
	case "publish":
		s, m, t, err := lookup()
		if err != nil {
			return err
		}
		r, err := client.PublishDeliveryOutcome(ctx, connect.NewRequest(&pb.PublishDeliveryOutcomeRequest{SessionId: s, MissionId: m, TaskId: t, ArtifactId: *artifact}))
		if err != nil {
			return err
		}
		version := uint32(0)
		if n := len(r.Msg.Versions); n > 0 {
			version = r.Msg.Versions[n-1].Number
		}
		fmt.Fprintf(stdout, "Published the verdict as artifact %s, version %d. Publish the next one with --id %s.\n", r.Msg.Id, version, r.Msg.Id)
		return nil
	case "review":
		if len(positional) != 2 || (positional[1] != "passed" && positional[1] != "failed") {
			return errors.New("lazybox delivery review <criterion> passed|failed")
		}
		if session == "" {
			return errors.New("lazybox delivery review runs from the reviewer agent of a Mission review")
		}
		r, err := client.RecordReviewCriterion(ctx, connect.NewRequest(&pb.RecordReviewCriterionRequest{SessionId: session, Criterion: positional[0], Result: positional[1]}))
		if err != nil {
			return err
		}
		fmt.Fprintln(stdout, "Recorded "+r.Msg.Note)
		return nil
	}
	fmt.Fprintln(stderr, deliveryUsage)
	return fmt.Errorf("unknown delivery verb %q", verb)
}

// outcomeText leads with the verdict on the headline candidate and what blocks
// it, then each criterion with its evidence; older candidates follow in one line each.
func outcomeText(o *pb.DeliveryOutcome) string {
	var b strings.Builder
	c := o.GetContract()
	fmt.Fprintf(&b, "Delivery against %s", specName(c.GetTitle(), c.GetArtifactId(), c.GetVersion()))
	if o.MissionId != "" {
		fmt.Fprintf(&b, " (Mission %s, task %s)", o.MissionId, o.TaskId)
	}
	b.WriteString("\n")
	if o.Decision != "" {
		fmt.Fprintf(&b, "Decision: %s (the technical verdict below is unchanged)\n", o.Decision)
	}
	if len(o.Candidates) == 0 {
		b.WriteString("Nothing checked yet, and the workspace's current files could not be read.\n")
		return b.String()
	}
	head := o.Candidates[0]
	where := "the workspace's current files"
	if !head.Current {
		where = "the latest files checked (the current ones could not be read)"
	}
	fmt.Fprintf(&b, "Verdict on %s (%s): %s\n", where, head.Candidate, head.Verdict)
	for _, criterion := range head.Criteria {
		level := "optional"
		if criterion.Required {
			level = "required"
		}
		fmt.Fprintf(&b, "  %-10s %s (%s): %s\n", criterion.State, criterion.Id, level, criterion.Expected)
		for _, e := range criterion.Evidence {
			line := e.State + " · " + firstNonEmpty(e.Title, e.Reference)
			if e.ExitCode != nil {
				line += fmt.Sprintf(" · exit %d", *e.ExitCode)
			}
			if e.Output == "unavailable" {
				line += " · output no longer kept"
			}
			fmt.Fprintf(&b, "             %s\n", line)
		}
	}
	for _, older := range o.Candidates[1:] {
		fmt.Fprintf(&b, "Earlier files %s: %s\n", older.Candidate, older.Verdict)
	}
	return b.String()
}

// specName is how a person reads a pinned spec: its title, else its artifact ID.
func specName(title, artifact string, version uint32) string {
	if title != "" {
		return fmt.Sprintf("“%s” v%d", title, version)
	}
	return fmt.Sprintf("spec %s v%d", artifact, version)
}
