// lazybox is the V2 local test client, transparent log publisher, and the
// `run` wrapper agents use for noisy commands (see run.go).
package main

import (
	"bytes"
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
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
	"github.com/codefly-dev/gortk"
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
	runnerName := global.String("runner", "", "registered Runner to use for power verbs (lazybox runners); LAZYBOX_RUNNER also selects one")
	global.Usage = func() {
		powerUsage(os.Stderr)
		fmt.Fprintln(os.Stderr)
		fmt.Fprintln(os.Stderr, "In a Lazybox workspace terminal no flag is needed: `cmd 2>&1 | lazybox log [--title CMD] [--to CONVERSATION]` or `lazybox run [--to CONVERSATION] -- cmd`. Agents publish plans and reports with `lazybox artifact put FILE --title T`.")
		fmt.Fprintln(os.Stderr, "Usage: lazybox --endpoint FILE <account-login|assistant|info|repositories|create-collection|collections|collection|archive-collection|reconcile-collection|open|send|interrupt|approve|answer|events|close|continue|exec|read|wait|cancel|output|log|run|process|artifact> [options]")
		global.PrintDefaults()
	}
	if len(os.Args) == 2 && (os.Args[1] == "--version" || os.Args[1] == "-version") {
		fmt.Println(version.Line())
		return nil
	}
	if err := global.Parse(os.Args[1:]); err != nil {
		return err
	}
	args := global.Args()
	if len(args) == 0 {
		global.Usage()
		return nil
	}
	// The power verbs (ls, send, open, watch, …) resolve their own Runner:
	// --endpoint names one explicitly; otherwise see endpoint.go.
	explicitEndpoint := ""
	global.Visit(func(f *flag.Flag) {
		if f.Name == "endpoint" {
			explicitEndpoint = f.Value.String()
		}
	})
	{
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		handled, err := powerVerb(newCLI(ctx, explicitEndpoint, *runnerName), args)
		stop()
		if handled {
			return err
		}
	}
	if args[0] == "account-login" {
		flags := flag.NewFlagSet("account-login", flag.ContinueOnError)
		state := flags.String("state", "", "Runner state directory (on the Runner host)")
		profile := flags.String("profile", "", "new subscription profile id")
		provider := flags.String("provider", "", "codex or claude")
		binary := flags.String("binary", "", "Codex CLI executable")
		tokenInput := flags.Bool("token-stdin", false, "refused: Lazybox does not store Claude subscription tokens (CLAUDE_COMPLIANCE.md)")
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
	if args[0] == "run" {
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		defer stop()
		return runVerb(ctx, *endpointPath, args[1:], os.Stdout, os.Stderr)
	}
	if args[0] == "process" {
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		defer stop()
		return processVerb(ctx, args[1:], os.Stdout, os.Stderr, func() (genconnect.RunnerServiceClient, error) {
			address, err := readEndpoint(*endpointPath)
			if err != nil {
				return nil, err
			}
			return genconnect.NewRunnerServiceClient(newHTTPClient(address.Token), address.URL), nil
		})
	}
	if args[0] == "artifact" {
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		defer stop()
		return artifactVerb(ctx, *endpointPath, args[1:], os.Stdin, os.Stdout, os.Stderr)
	}
	if args[0] == "log" {
		// From a person's own terminal, the registered Runner (pipe_target.go).
		*endpointPath = ownerEndpoint(os.Getenv, *endpointPath, *runnerName)
	}
	if *endpointPath == "" {
		return preserveOnFailure(args[0], errors.New("no Runner endpoint: open Lazybox or start a Runner (lazybox runners lists them), or pass --endpoint; no V1 state is used"), os.Stdin, os.Stdout)
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
	rotate := flags.String("rotate-profiles", "", "comma-separated isolated profiles this conversation may switch to (only with an account policy, EXTENSIONS.md; default: its accounts)")
	noRotate := flags.Bool("no-rotate", false, "keep this conversation on --profile; never switch subscriptions automatically")
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
	title := flags.String("title", "", "console title; for log, the producer's command line also picks the compact view's filter (default: \"command\", or \"terminal output\" in a workspace terminal)")
	to := flags.String("to", "", "log from a workspace terminal: also put the compact view in this conversation's message box (id or title)")
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
		return output(client.OpenAssistant(ctx, connect.NewRequest(&pb.OpenAssistantRequest{ProfileId: *profile, Purpose: *purpose, Context: contextText, Options: &pb.RunnerOptions{Model: *model, GatewayId: *gateway, RotationProfileIds: rotationProfiles, NoRotation: *noRotate}})))
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
		return output(client.OpenSession(ctx, connect.NewRequest(&pb.OpenSessionRequest{ProfileId: *profile, Cwd: *cwd, CollectionId: *collection, ResumeReference: *resume, Options: &pb.RunnerOptions{Model: *model, Workflow: *workflow, Permission: *permission, GatewayId: *gateway, RotationProfileIds: rotationProfiles, NoRotation: *noRotate}})))
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
		if *title == "" {
			*title = "command"
		}
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
		if *session == "" && !scopedEnvironment(os.Getenv) {
			// A workspace by name, or the one the current directory is in.
			cwd, _ := os.Getwd()
			if *collection, err = resolveWorkspace(ctx, client, *collection, cwd); err != nil {
				fmt.Fprintln(os.Stderr, "lazybox:", err)
			}
		}
		return publish(ctx, client, pipeTarget{session: *session, collection: *collection, to: *to}, *title, os.Getenv("LAZYBOX_V2_OUTPUT_DIR"), os.Stdin, os.Stdout, os.Stderr)
	default:
		return fmt.Errorf("unknown command %s", verb)
	}
}

// Publication failure must not swallow the producer's output. Drain and echo
// stdin even after the RPC has failed, then return a failure.
//
// From a workspace terminal (no session) the run belongs to the workspace and
// a person may hand it to an agent, so it also keeps what `lazybox run` keeps:
// the full output in a file and a compact, failure-first view of it, sent with
// the capture's completion. The terminal still receives every byte.
func publish(ctx context.Context, client console, target pipeTarget, title, outputDir string, input io.Reader, stdout, stderr io.Writer) (resultErr error) {
	workspace := target.workspace()
	if title == "" {
		title = "command"
		if workspace {
			title = "terminal output"
		}
	}
	request, publicationErr := target.request(title)
	if publicationErr != nil && target.to != "" {
		// The producer is already running: keep its output, refuse the share.
		_, err := io.Copy(stdout, input)
		return errors.Join(publicationErr, err)
	}
	id := ""
	var capture *sink
	started := time.Now()
	var shareErr error
	if publicationErr == nil {
		var r *connect.Response[pb.CommandStatus]
		r, publicationErr = client.RegisterPipe(ctx, connect.NewRequest(request))
		if publicationErr != nil && request.ShareWith != "" {
			// The producer is already running: a wrong --to must not cost
			// its output a console. Publish it unshared and say why.
			shareErr, target.to = publicationErr, ""
			request = &pb.RegisterPipeRequest{CollectionId: request.CollectionId, Title: request.Title}
			r, publicationErr = client.RegisterPipe(ctx, connect.NewRequest(request))
		}
		if publicationErr == nil {
			id = r.Msg.Id
		}
	}
	if shareErr != nil {
		defer fmt.Fprintf(stderr, "[lazybox] not shared: %v\n", shareErr)
	}
	if publicationErr == nil {
		if workspace {
			capture = &sink{}
			if f, path, err := outputFile(outputDir, "", title); err == nil {
				capture.file, capture.path = f, path
				defer f.Close()
			}
		} else {
			fmt.Fprintln(stderr, "console command:", id)
		}
		defer func() {
			finishCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), 5*time.Second)
			defer cancel()
			cause := ""
			if resultErr != nil {
				cause = resultErr.Error()
			} else if publicationErr != nil {
				cause = publicationErr.Error()
			}
			finish := &pb.FinishPipeRequest{CommandId: id, CaptureError: cause}
			if capture != nil {
				finish.Report = capture.pipedReport(title, time.Since(started))
			}
			_, finishErr := client.FinishPipe(finishCtx, connect.NewRequest(finish))
			if resultErr == nil {
				resultErr = finishErr
			}
			if capture != nil {
				fmt.Fprintln(stderr, workspaceNote(title, target.to, finish.Report, id, resultErr))
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
			if capture != nil {
				_, _ = capture.stream(&capture.out).Write(buffer[:n])
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

// pipedReport is what an agent would receive for a piped run: gortk picks its
// filter from the title, so `--title "go test ./..."` gets the go-test view.
func (s *sink) pipedReport(title string, elapsed time.Duration) *pb.CommandReport {
	s.mu.Lock()
	defer s.mu.Unlock()
	result := compressor.Compress(gortk.CommandFromLine(title, s.out.data, nil, 0))
	var view bytes.Buffer
	writeReport(&view, report{piped: true, line: title, elapsed: elapsed, result: result, path: s.path, bytes: s.total, clipped: s.out.clipped})
	return &pb.CommandReport{Piped: true, OutputBytes: uint64(s.total), AgentBytes: uint64(view.Len()), Filter: result.Filter, OutputPath: s.path, AgentView: boundView(view.String())}
}

type processExit int32

func (e processExit) Error() string { return "command exited " + strconv.Itoa(int(e)) }
func main() {
	if err := run(); err != nil {
		var coded exitError
		if errors.As(err, &coded) {
			if coded.code == exitOK {
				return
			}
			fmt.Fprintln(os.Stderr, "lazybox:", err)
			os.Exit(coded.code)
		}
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
