// lazybox is the V2 local test client and transparent log publisher.
package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"github.com/AntoineToussaint/lazybox/v2/pkg/gen/genconnect"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

type endpoint struct {
	URL   string `json:"url"`
	Token string `json:"token"`
}
type authTransport struct{ token string }

func (a authTransport) RoundTrip(r *http.Request) (*http.Response, error) {
	copy := r.Clone(r.Context())
	copy.Header = r.Header.Clone()
	copy.Header.Set("Authorization", "Bearer "+a.token)
	return http.DefaultTransport.RoundTrip(copy)
}
func printProto(p proto.Message) error {
	b, err := protojson.Marshal(p)
	if err != nil {
		return err
	}
	_, err = fmt.Fprintln(os.Stdout, string(b))
	return err
}
func output[T any](r *connect.Response[T], err error) error {
	if err != nil {
		return err
	}
	p, ok := any(r.Msg).(proto.Message)
	if !ok {
		return errors.New("invalid response")
	}
	return printProto(p)
}

func run() error {
	global := flag.NewFlagSet("lazybox", flag.ContinueOnError)
	endpointPath := global.String("endpoint", os.Getenv("LAZYBOX_V2_ENDPOINT"), "endpoint.json printed by the V2 runner")
	global.Usage = func() {
		fmt.Fprintln(os.Stderr, "Usage: lazybox --endpoint FILE <account-login|assistant|info|repositories|create-collection|collections|collection|archive-collection|reconcile-collection|open|send|interrupt|approve|answer|events|close|continue|exec|read|wait|cancel|output|log> [options]")
		global.PrintDefaults()
	}
	if err := global.Parse(os.Args[1:]); err != nil {
		return err
	}
	args := global.Args()
	if len(args) == 0 {
		global.Usage()
		return nil
	}
	if args[0] == "account-login" {
		flags := flag.NewFlagSet("account-login", flag.ContinueOnError)
		state := flags.String("state", "", "Runner state directory (on the Runner host)")
		profile := flags.String("profile", "", "new subscription profile id")
		provider := flags.String("provider", "", "codex or claude")
		binary := flags.String("binary", "", "Codex CLI executable")
		tokenInput := flags.Bool("token-stdin", false, "read Claude subscription setup-token privately from stdin")
		if err := flags.Parse(args[1:]); err != nil {
			return err
		}
		var token io.Reader
		if *tokenInput {
			token = os.Stdin
		}
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		defer stop()
		p, err := accounts.Enroll(ctx, *state, *profile, *provider, *binary, token, os.Stdin, os.Stdout, os.Stderr)
		if err != nil {
			return err
		}
		fmt.Fprintf(os.Stdout, "\nEnrolled %s (%s). Restart the Runner to load this profile.\n", p.ID, p.Provider)
		return nil
	}
	if *endpointPath == "" {
		return preserveOnFailure(args[0], errors.New("--endpoint or LAZYBOX_V2_ENDPOINT is required; no V1 state is used"), os.Stdin, os.Stdout)
	}
	data, err := os.ReadFile(*endpointPath)
	if err != nil {
		return preserveOnFailure(args[0], err, os.Stdin, os.Stdout)
	}
	var address endpoint
	if err = json.Unmarshal(data, &address); err != nil {
		return preserveOnFailure(args[0], err, os.Stdin, os.Stdout)
	}
	client := genconnect.NewRunnerServiceClient(&http.Client{Transport: authTransport{address.Token}}, address.URL)
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	verb := args[0]
	flags := flag.NewFlagSet(verb, flag.ContinueOnError)
	session := flags.String("session", os.Getenv("LAZYBOX_V2_SESSION_ID"), "execution id")
	id := flags.String("id", "", "command id, or idempotent operation id for agent operations")
	profile := flags.String("profile", "", "configured account profile id")
	rotate := flags.String("rotate-profiles", "", "comma-separated isolated profiles explicitly enabled for rotation")
	purpose := flags.String("purpose", "help", "assistant purpose: help, planning, summary")
	contextFile := flags.String("context", "", "assistant context text file")
	cwd := flags.String("cwd", "", "absolute working directory")
	collection := flags.String("collection", os.Getenv("LAZYBOX_V2_COLLECTION_ID"), "Runner collection id (instead of cwd)")
	spec := flags.String("spec", "", "CreateCollectionRequest JSON file, including a stable requestKey")
	owner := flags.String("owner", "", "expected previous execution owner for reconciliation")
	stopped := flags.Bool("processes-stopped", false, "operator confirms previous collection agent and all its commands have stopped")
	workflow := flags.String("workflow", "", "execute or plan")
	model := flags.String("model", "", "provider model (default: runner/provider default)")
	permission := flags.String("permission", "", "ask or read-only")
	gateway := flags.String("gateway", "", "configured gateway id, or direct")
	resume := flags.String("resume", "", "native provider conversation reference")
	text := flags.String("text", "", "message text")
	image := flags.String("image", "", "image file to attach")
	steer := flags.Bool("steer", false, "steer active turn if adapter supports it")
	request := flags.String("request", "", "native pending request id")
	decision := flags.String("decision", "decline", "accept, decline, cancel")
	answers := flags.String("answers", "{}", "JSON map of question ids to arrays of answers")
	after := flags.Uint64("after", 0, "replay journal cursor")
	offset := flags.Uint64("offset", 0, "output byte offset")
	limit := flags.Uint("limit", 65536, "maximum read bytes")
	title := flags.String("title", "command", "console title")
	wait := flags.Bool("wait", false, "wait for process and return its actual exit status")
	mode := flags.String("mode", "native", "native or checkpoint continuation")
	reason := flags.String("reason", "", "continuation reason")
	checkpoint := flags.String("checkpoint", "", "checkpoint JSON file using the generated Checkpoint schema")
	if err = flags.Parse(args[1:]); err != nil {
		return err
	}
	operation := func() *pb.ApplyRequest {
		operationID := *id
		if operationID == "" {
			operationID = commands.ID()
		}
		return &pb.ApplyRequest{SessionId: *session, OperationId: operationID}
	}
	rotationProfiles := []string{}
	if *rotate != "" {
		rotationProfiles = strings.Split(*rotate, ",")
	}
	switch verb {
	case "assistant":
		contextText := ""
		if *contextFile != "" {
			data, err := os.ReadFile(*contextFile)
			if err != nil {
				return err
			}
			contextText = string(data)
		}
		return output(client.OpenAssistant(ctx, connect.NewRequest(&pb.OpenAssistantRequest{ProfileId: *profile, Purpose: *purpose, Context: contextText, Options: &pb.RunnerOptions{Model: *model, GatewayId: *gateway, RotationProfileIds: rotationProfiles}})))
	case "info":
		return output(client.Info(ctx, connect.NewRequest(&pb.InfoRequest{})))
	case "repositories":
		return output(client.ListRepositories(ctx, connect.NewRequest(&pb.ListRepositoriesRequest{})))
	case "collections":
		return output(client.ListCollections(ctx, connect.NewRequest(&pb.ListCollectionsRequest{})))
	case "collection":
		return output(client.GetCollection(ctx, connect.NewRequest(&pb.CollectionRequest{CollectionId: *collection})))
	case "archive-collection":
		return output(client.ArchiveCollection(ctx, connect.NewRequest(&pb.CollectionRequest{CollectionId: *collection})))
	case "reconcile-collection":
		return output(client.ReconcileCollection(ctx, connect.NewRequest(&pb.ReconcileCollectionRequest{CollectionId: *collection, ExpectedOwnerExecutionId: *owner, ProcessesStopped: *stopped, Reason: *reason})))
	case "create-collection":
		data, err := os.ReadFile(*spec)
		if err != nil {
			return err
		}
		request := &pb.CreateCollectionRequest{}
		if err = protojson.Unmarshal(data, request); err != nil {
			return err
		}
		return output(client.CreateCollection(ctx, connect.NewRequest(request)))
	case "open":
		if *cwd == "" && *collection == "" {
			*cwd, err = os.Getwd()
			if err != nil {
				return err
			}
		}
		if *cwd != "" {
			*cwd, err = filepath.Abs(*cwd)
			if err != nil {
				return err
			}
		}
		return output(client.OpenSession(ctx, connect.NewRequest(&pb.OpenSessionRequest{ProfileId: *profile, Cwd: *cwd, CollectionId: *collection, ResumeReference: *resume, Options: &pb.RunnerOptions{Model: *model, Workflow: *workflow, Permission: *permission, GatewayId: *gateway, RotationProfileIds: rotationProfiles}})))
	case "send":
		message := &pb.Message{Text: *text}
		if *image != "" {
			data, err := os.ReadFile(*image)
			if err != nil {
				return err
			}
			media := http.DetectContentType(data)
			message.Images = append(message.Images, &pb.Image{MediaType: media, Base64Data: base64.StdEncoding.EncodeToString(data)})
		}
		p := operation()
		p.Operation = &pb.ApplyRequest_Message{Message: message}
		if *steer {
			p.Operation = &pb.ApplyRequest_Steer{Steer: message}
		}
		return output(client.Apply(ctx, connect.NewRequest(p)))
	case "interrupt":
		p := operation()
		p.Operation = &pb.ApplyRequest_Interrupt{Interrupt: &pb.Interrupt{}}
		return output(client.Apply(ctx, connect.NewRequest(p)))
	case "approve":
		choice := map[string]pb.Decision{"accept": pb.Decision_DECISION_ACCEPT, "decline": pb.Decision_DECISION_DECLINE, "cancel": pb.Decision_DECISION_CANCEL}[*decision]
		if choice == pb.Decision_DECISION_UNSPECIFIED {
			return errors.New("invalid decision")
		}
		p := operation()
		p.Operation = &pb.ApplyRequest_Approval{Approval: &pb.Approval{RequestId: *request, Decision: choice}}
		return output(client.Apply(ctx, connect.NewRequest(p)))
	case "answer":
		var values map[string][]string
		if err = json.Unmarshal([]byte(*answers), &values); err != nil {
			return err
		}
		answer := &pb.Answer{RequestId: *request, Answers: map[string]*pb.Answers{}}
		for k, v := range values {
			answer.Answers[k] = &pb.Answers{Values: v}
		}
		p := operation()
		p.Operation = &pb.ApplyRequest_Answer{Answer: answer}
		return output(client.Apply(ctx, connect.NewRequest(p)))
	case "close":
		return output(client.CloseSession(ctx, connect.NewRequest(&pb.SessionRequest{SessionId: *session})))
	case "continue":
		p := &pb.ContinueSessionRequest{SessionId: *session, ProfileId: *profile, Mode: *mode, Reason: *reason}
		if *checkpoint != "" {
			data, err := os.ReadFile(*checkpoint)
			if err != nil {
				return err
			}
			p.Checkpoint = &pb.Checkpoint{}
			if err = protojson.Unmarshal(data, p.Checkpoint); err != nil {
				return err
			}
		}
		return output(client.ContinueSession(ctx, connect.NewRequest(p)))
	case "events":
		stream, err := client.WatchEvents(ctx, connect.NewRequest(&pb.WatchEventsRequest{SessionId: *session, After: *after}))
		if err != nil {
			return err
		}
		defer stream.Close()
		for stream.Receive() {
			if err = printProto(stream.Msg()); err != nil {
				return err
			}
		}
		return stream.Err()
	case "exec":
		r, err := client.StartCommand(ctx, connect.NewRequest(&pb.StartCommandRequest{SessionId: *session, Title: *title, Argv: flags.Args()}))
		if err != nil {
			return err
		}
		if err = printProto(r.Msg); err != nil {
			return err
		}
		if *wait {
			result, err := client.WaitCommand(ctx, connect.NewRequest(&pb.CommandRequest{CommandId: r.Msg.Id}))
			if err != nil {
				return err
			}
			if err = printProto(result.Msg); err != nil {
				return err
			}
			if result.Msg.CaptureError != "" {
				return errors.New(result.Msg.CaptureError)
			}
			if result.Msg.ExitCode != nil && *result.Msg.ExitCode != 0 {
				return processExit(*result.Msg.ExitCode)
			}
		}
		return nil
	case "read":
		r, err := client.ReadCommand(ctx, connect.NewRequest(&pb.ReadCommandRequest{CommandId: *id, Offset: *offset, Limit: uint32(*limit)}))
		if err != nil {
			return err
		}
		_, err = os.Stdout.Write(r.Msg.Data)
		return err
	case "wait":
		return output(client.WaitCommand(ctx, connect.NewRequest(&pb.CommandRequest{CommandId: *id})))
	case "cancel":
		return output(client.CancelCommand(ctx, connect.NewRequest(&pb.CommandRequest{CommandId: *id})))
	case "output":
		stream, err := client.WatchOutput(ctx, connect.NewRequest(&pb.WatchOutputRequest{CommandId: *id, After: *after}))
		if err != nil {
			return err
		}
		defer stream.Close()
		for stream.Receive() {
			if err = printProto(stream.Msg()); err != nil {
				return err
			}
		}
		return stream.Err()
	case "log":
		return publish(ctx, client, *session, *title, os.Stdin, os.Stdout, os.Stderr)
	default:
		return fmt.Errorf("unknown command %s", verb)
	}
}

// Publication failure must not swallow the producer's agent-facing output.
// Drain and echo stdin even after the RPC has failed, then return a failure.
func publish(ctx context.Context, client genconnect.RunnerServiceClient, session, title string, input io.Reader, stdout, stderr io.Writer) (resultErr error) {
	r, publicationErr := client.RegisterPipe(ctx, connect.NewRequest(&pb.RegisterPipeRequest{SessionId: session, Title: title}))
	id := ""
	if publicationErr == nil {
		id = r.Msg.Id
		fmt.Fprintln(stderr, "console command:", id)
		defer func() {
			finishCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), 5*time.Second)
			defer cancel()
			cause := ""
			if resultErr != nil {
				cause = resultErr.Error()
			} else if publicationErr != nil {
				cause = publicationErr.Error()
			}
			_, finishErr := client.FinishPipe(finishCtx, connect.NewRequest(&pb.FinishPipeRequest{CommandId: id, CaptureError: cause}))
			if resultErr == nil {
				resultErr = finishErr
			}
		}()
	}
	buffer := make([]byte, 16<<10)
	offset := uint64(0)
	for {
		n, readErr := input.Read(buffer)
		if n > 0 {
			written, err := stdout.Write(buffer[:n])
			if err != nil {
				return err
			}
			if written != n {
				return io.ErrShortWrite
			}
			if publicationErr == nil {
				_, publicationErr = client.Publish(ctx, connect.NewRequest(&pb.PublishRequest{CommandId: id, Offset: offset, Data: buffer[:n]}))
				if publicationErr == nil {
					offset += uint64(n)
				}
			}
		}
		if readErr != nil {
			if readErr != io.EOF && publicationErr == nil {
				publicationErr = readErr
			}
			break
		}
	}
	if publicationErr != nil {
		return fmt.Errorf("console publication failed (stdin was preserved): %w", publicationErr)
	}
	return nil
}

type processExit int32

func (e processExit) Error() string { return "command exited " + strconv.Itoa(int(e)) }
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		var status processExit
		if errors.As(err, &status) {
			code := int(status)
			if code < 1 || code > 125 {
				code = 1
			}
			os.Exit(code)
		}
		os.Exit(1)
	}
}

func preserveOnFailure(verb string, cause error, input io.Reader, output io.Writer) error {
	if verb == "log" {
		_, err := io.Copy(output, input)
		return errors.Join(cause, err)
	}
	return cause
}
