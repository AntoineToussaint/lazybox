package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"

	"connectrpc.com/connect"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

type deliveryAPI interface {
	BindDelivery(context.Context, *connect.Request[pb.BindDeliveryRequest]) (*connect.Response[pb.DeliveryBinding], error)
}

const deliveryUsage = `Usage:
  lazybox delivery bind <spec-id>@<version>

Pins a published spec.v2 version to this conversation (start one with
lazybox artifact template delivery-spec). Then its runs record evidence:
  lazybox run --check <criterion> -- <command>   exit 0 records a pass, any other exit a failure
  lazybox run --needs <criterion> -- <command>   a non-zero exit records a failed prerequisite (blocked)
A conversation working a Mission task checks against the plan's approved
contract and needs no bind. Binding another version starts a new attempt.`

func deliveryVerb(ctx context.Context, endpointPath string, args []string, stdout, stderr io.Writer) error {
	address, err := readEndpoint(endpointPath)
	if err != nil {
		return err
	}
	client := genconnect.NewRunnerServiceClient(newHTTPClient(address.Token), address.URL)
	return deliveryCommand(ctx, client, os.Getenv("LAZYBOX_V2_SESSION_ID"), args, stdout, stderr)
}

func deliveryCommand(ctx context.Context, client deliveryAPI, session string, args []string, stdout, stderr io.Writer) error {
	if len(args) != 2 || args[0] != "bind" {
		fmt.Fprintln(stderr, deliveryUsage)
		return errors.New("lazybox delivery needs: bind <spec-id>@<version>")
	}
	if session == "" {
		return errors.New("lazybox delivery bind runs from a Lazybox agent: it binds that agent's conversation")
	}
	id, v, ok := strings.Cut(args[1], "@")
	version, err := strconv.ParseUint(v, 10, 32)
	if !ok || id == "" || err != nil || version == 0 {
		return fmt.Errorf("write the spec as <spec-id>@<version>, for example %s@3", firstNonEmpty(id, "spec-id"))
	}
	r, err := client.BindDelivery(ctx, connect.NewRequest(&pb.BindDeliveryRequest{SessionId: session, ArtifactId: id, Version: uint32(version)}))
	if err != nil {
		return err
	}
	b := r.Msg
	fmt.Fprintf(stdout, "Bound spec %s v%d to this conversation (attempt %s).\nCheck each criterion with lazybox run --check <id> -- <command>:\n", b.ArtifactId, b.Version, b.Attempt)
	for _, c := range b.Criteria {
		fmt.Fprintln(stdout, "- "+c)
	}
	return nil
}
