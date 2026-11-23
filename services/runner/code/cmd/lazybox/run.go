package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
	"github.com/codefly-dev/gortk"
)

// `lazybox run -- COMMAND` is how an agent runs a noisy command. The human
// console receives every byte live, the full output is saved to a file, and
// the agent receives a compact, failure-first view plus that file's path.
// The command's exit status is returned unchanged.
//
// From a workspace terminal (no session, a workspace) the person is the
// reader: the output streams to the terminal as usual, the run appears in the
// console of every conversation in the workspace, and the compact view is
// kept with it so the person can hand it to an agent (`--to` does that now).

// console is the part of the Runner API a run publishes to.
type console interface {
	RegisterPipe(context.Context, *connect.Request[pb.RegisterPipeRequest]) (*connect.Response[pb.CommandStatus], error)
	Publish(context.Context, *connect.Request[pb.PublishRequest]) (*connect.Response[pb.CommandStatus], error)
	FinishPipe(context.Context, *connect.Request[pb.FinishPipeRequest]) (*connect.Response[pb.CommandStatus], error)
}

// compressor is the Runner's (commands.Compressor): every compact view an
// agent receives comes from the same filter.
var compressor = commands.Compressor

// captureLimit bounds what each stream keeps in memory for compression. The
// console and the full-output file are not bounded by it.
const captureLimit = 32 << 20

type runOptions struct {
	session, title, outputDir string
	// collection and to make a workspace run (see pipeTarget).
	collection, to string
	raw            bool
	// grace is how long a command runs before it may go on in the background
	// (zero selects defaultGrace); wait keeps it in the foreground until it
	// exits (--wait); background moves it as soon as it has started
	// (--background). See run_background.go.
	grace      time.Duration
	wait       bool
	background bool
	// endpoint is the Runner's, for the process that follows a command in the
	// background; zero without a Runner.
	endpoint endpoint
}

// pipeTarget is where a run or a log is published: an execution's console
// (an agent), or its workspace (a person in a workspace terminal), optionally
// shared with one conversation of that workspace.
type pipeTarget struct{ session, collection, to string }

// workspace reports a run published from a workspace terminal.
func (t pipeTarget) workspace() bool { return t.session == "" && t.collection != "" }

func (t pipeTarget) request(title string) (*pb.RegisterPipeRequest, error) {
	switch {
	case t.session != "" && t.to != "":
		return nil, errors.New("--to shares a run from a workspace terminal; an agent's own runs already reach it")
	case t.session != "":
		return &pb.RegisterPipeRequest{SessionId: t.session, Title: title}, nil
	case t.collection != "":
		return &pb.RegisterPipeRequest{CollectionId: t.collection, Title: title, ShareWith: t.to}, nil
	}
	return nil, errors.New("no Lazybox session or workspace (run it from a Lazybox agent or workspace terminal, or pass --session or --collection)")
}

// runVerb parses `run` flags and connects to the Runner when it can. An
// unreachable Runner never stops the command: the agent still gets its output.
func runVerb(ctx context.Context, endpointPath string, args []string, stdout, stderr io.Writer) error {
	flags := flag.NewFlagSet("run", flag.ContinueOnError)
	flags.SetOutput(stderr)
	o := runOptions{}
	flags.StringVar(&o.session, "session", os.Getenv("LAZYBOX_V2_SESSION_ID"), "execution id whose console shows the output")
	flags.StringVar(&o.collection, "collection", os.Getenv("LAZYBOX_V2_COLLECTION_ID"), "workspace whose console shows the output, when there is no session (set in workspace terminals)")
	flags.StringVar(&o.to, "to", "", "from a workspace terminal: also put the compact view in this conversation's message box (id or title)")
	flags.StringVar(&o.title, "title", "", "console title (default: the command line)")
	flags.StringVar(&o.outputDir, "output-dir", os.Getenv("LAZYBOX_V2_OUTPUT_DIR"), "directory for full-output files (default: the session's, set by the Runner; else system temp)")
	flags.BoolVar(&o.raw, "raw", false, "give the agent the full output instead of the compact view")
	wait := &waitFlag{o: &o}
	flags.Var(wait, "wait", "keep a server or watcher in the foreground until it exits; --wait=DURATION waits that long before moving it to the background (default 20s)")
	flags.BoolVar(&o.background, "background", false, "move the command to the background as soon as it has started (it keeps running; lazybox stop ID ends it)")
	flags.Usage = func() {
		fmt.Fprintln(stderr, "Usage: lazybox run [--title T] [--raw] [--wait[=DURATION] | --background] [--to CONVERSATION] -- COMMAND [ARGS...]")
		fmt.Fprintln(stderr, "A command still running after 20s that listens on a TCP port or looks like a server or watcher (npm run dev, docker compose up, --watch, …) goes on in the background: lazybox run returns with its id, port and output file.")
		flags.PrintDefaults()
	}
	if err := flags.Parse(args); err != nil {
		return err
	}
	if rest := flags.Args(); o.wait && len(rest) > 1 && rest[1] == "--" {
		if _, err := time.ParseDuration(rest[0]); err == nil {
			return fmt.Errorf("write --wait=%s: a bare --wait waits for the exit", rest[0])
		}
	}
	if o.wait && o.background {
		return errors.New("--wait and --background contradict each other")
	}
	var c console
	var unavailable error
	// From a person's own terminal: the registered Runner, and the workspace
	// by name or by the current directory (pipe_target.go).
	endpointPath = ownerEndpoint(os.Getenv, endpointPath, os.Getenv("LAZYBOX_RUNNER"))
	if address, err := readEndpoint(endpointPath); err != nil {
		unavailable = err
	} else {
		o.endpoint = address
		client := genconnect.NewRunnerServiceClient(newHTTPClient(address.Token), address.URL)
		c = client
		if o.session == "" && !scopedEnvironment(os.Getenv) {
			cwd, _ := os.Getwd()
			if o.collection, err = resolveWorkspace(ctx, client, o.collection, cwd); err != nil {
				fmt.Fprintln(stderr, "lazybox:", err)
			}
		}
	}
	return runCommand(ctx, c, unavailable, o, flags.Args(), stdout, stderr)
}

func runCommand(ctx context.Context, c console, unavailable error, o runOptions, argv []string, stdout, stderr io.Writer) error {
	if len(argv) == 0 {
		return errors.New("usage: lazybox run [--title T] [--raw] -- COMMAND [ARGS...]")
	}
	line := strings.Join(argv, " ")
	title := o.title
	if title == "" {
		title = shorten(line, 120)
	}
	target := pipeTarget{o.session, o.collection, o.to}
	request, targetErr := target.request(title)
	if request != nil && request.SessionId != "" {
		request.CreatesPullRequest = workitems.CreatesPullRequestArgv(argv)
	}
	if o.to != "" && targetErr != nil {
		return targetErr // refused before running anything
	}
	s := &sink{}
	if target.workspace() {
		// A person is watching: everything reaches the terminal as it arrives.
		s.out.echo, s.err.echo = stdout, stderr
	}
	if f, path, err := outputFile(o.outputDir, o.session, argv[0]); err == nil {
		s.file, s.path = f, path
		defer f.Close()
	} else {
		fmt.Fprintln(stderr, "lazybox: full output will not be saved:", err)
	}
	if c == nil {
		s.publishErr = unavailable
	} else if targetErr != nil {
		s.publishErr = targetErr
	} else if r, err := c.RegisterPipe(ctx, connect.NewRequest(request)); err != nil {
		s.publishErr = err
		if o.to != "" {
			return fmt.Errorf("not shared, nothing was run: %w", err) // a wrong --to is fixed before the run, not after
		}
	} else {
		s.ctx, s.console, s.id = ctx, c, r.Msg.Id
	}

	started := time.Now()
	f := &foreground{ctx: ctx, c: c, o: o, target: target, argv: argv, line: line, title: title, s: s, stdout: stdout, stderr: stderr, started: started}
	code, moved, err := f.run()
	if moved || err != nil {
		return err // It runs on in the background; the report is written.
	}
	elapsed := time.Since(started)

	var result gortk.Result
	if o.raw {
		result = gortk.Result{Text: string(s.out.data) + string(s.err.data), Filter: "raw"}
	} else {
		result = compressor.Compress(gortk.CommandFromArgs(argv, s.out.data, s.err.data, code))
	}
	// The view is built before the capture closes so the console can label the
	// run with what the agent received.
	var view bytes.Buffer
	published := s.publishErr
	r := report{line: line, code: code, elapsed: elapsed, result: result, path: s.path, bytes: s.total, clipped: s.out.clipped || s.err.clipped, console: s.id}
	if !target.workspace() {
		r.publishErr = published // a person sees it on stderr instead
	}
	writeReport(&view, r)
	summary := &pb.CommandReport{ExitCode: int32(code), OutputBytes: uint64(s.total), AgentBytes: uint64(view.Len()), Filter: result.Filter, OutputPath: s.path}
	if target.workspace() {
		summary.AgentView = boundView(view.String())
	}
	s.finish(summary)
	if target.workspace() {
		// The person already saw every line; say where the run went.
		fmt.Fprintln(stderr, workspaceNote(title, o.to, summary, s.id, s.publishErr))
		if code != 0 {
			return processExit(code)
		}
		return nil
	}
	if published == nil && s.publishErr != nil {
		fmt.Fprintf(&view, "[lazybox] not shown in the Lazybox console: %v\n", s.publishErr)
	}
	_, _ = stdout.Write(view.Bytes()) // the command's own status matters more than a closed stdout
	if code != 0 {
		return processExit(code)
	}
	return nil
}

// boundView keeps a compact view within what a report may carry (64 KiB).
// gortk already bounds its part to 32 KiB, so this only guards the notes.
func boundView(view string) string {
	const limit = 64 << 10
	if len(view) <= limit {
		return view
	}
	return view[:limit-64] + "\n[lazybox] the compact view was cut at 64 KiB\n"
}

// workspaceNote is the one line a person sees after a run or log from a
// workspace terminal: where the run went and what an agent would receive.
func workspaceNote(title, to string, r *pb.CommandReport, console string, publishErr error) string {
	if publishErr != nil || console == "" {
		if publishErr == nil {
			publishErr = errors.New("no console")
		}
		return fmt.Sprintf("[lazybox] not shown in the Lazybox console: %v", publishErr)
	}
	where := "is in the workspace console"
	if to != "" {
		where = fmt.Sprintf("is in the workspace console, and its compact view is in the message box of %q for you to send", to)
	}
	return fmt.Sprintf("[lazybox] %q %s (an agent gets %s of %s, %s)", shorten(title, 60), where, size(int64(r.AgentBytes)), size(int64(r.OutputBytes)), filterName(r.Filter))
}

func filterName(filter string) string {
	switch filter {
	case "", "passthrough":
		return "no filter"
	case "raw":
		return "raw output"
	}
	return filter + " filter"
}

type report struct {
	// piped: the output came through `lazybox log`, so there is no exit status.
	piped      bool
	line       string
	code       int
	elapsed    time.Duration
	result     gortk.Result
	path       string
	bytes      int64
	clipped    bool
	console    string
	publishErr error
}

// writeReport puts the outcome first so an agent that reads only the top
// still sees whether the command failed, then the compact output, then how
// to get the rest. Agents rarely fetch more, so failures must survive here.
func writeReport(w io.Writer, r report) {
	if r.piped {
		fmt.Fprintf(w, "[lazybox] %s → piped from a terminal, exit status unknown (%s)\n", shorten(r.line, 200), r.elapsed.Round(100*time.Millisecond))
	} else {
		fmt.Fprintf(w, "[lazybox] %s → exit %d (%s)\n", shorten(r.line, 200), r.code, r.elapsed.Round(100*time.Millisecond))
	}
	text := r.result.Text
	if text != "" && !strings.HasSuffix(text, "\n") {
		text += "\n"
	}
	io.WriteString(w, text)
	t := r.result.Truncation
	lossy := !r.result.Lossless() || r.clipped
	if lossy {
		note := t.Note
		if note == "" {
			note = "some output was omitted"
		}
		if r.clipped {
			note += fmt.Sprintf("; the compact view covers the first %d MiB of each stream", captureLimit>>20)
		}
		fmt.Fprintf(w, "[lazybox] %s\n", note)
	}
	if r.path != "" && (lossy || r.code != 0 || r.piped) {
		fmt.Fprintf(w, "[lazybox] full output (%s): %s\n", size(r.bytes), r.path)
	}
	if r.publishErr != nil {
		fmt.Fprintf(w, "[lazybox] not shown in the Lazybox console: %v\n", r.publishErr)
	}
}

// sink fans one command's output out to the console, the full-output file and
// a bounded per-stream buffer. exec writes stdout and stderr from separate
// goroutines, so every write is serialized.
type sink struct {
	mu         sync.Mutex
	ctx        context.Context
	console    console
	id         string
	offset     uint64
	publishErr error
	file       *os.File
	path       string
	total      int64
	out, err   buffer
	// tail is the end of both streams as they arrived, for the report of a
	// run that goes on in the background.
	tail []byte
}

// tailLimit bounds sink.tail.
const tailLimit = 16 << 10

type buffer struct {
	data    []byte
	clipped bool
	// echo, for a person's run, shows the stream in the terminal as it arrives.
	echo io.Writer
}

type streamWriter struct {
	s *sink
	b *buffer
}

func (s *sink) stream(b *buffer) io.Writer { return streamWriter{s, b} }

func (w streamWriter) Write(p []byte) (int, error) {
	s := w.s
	s.mu.Lock()
	defer s.mu.Unlock()
	s.total += int64(len(p))
	s.tail = append(s.tail, p...)
	if over := len(s.tail) - tailLimit; over > 0 {
		s.tail = append(s.tail[:0], s.tail[over:]...)
	}
	if w.b.echo != nil {
		_, _ = w.b.echo.Write(p)
	}
	if room := captureLimit - len(w.b.data); room >= len(p) {
		w.b.data = append(w.b.data, p...)
	} else {
		w.b.data = append(w.b.data, p[:max(room, 0)]...)
		w.b.clipped = true
	}
	if s.file != nil {
		if _, err := s.file.Write(p); err != nil {
			s.file, s.path = nil, ""
		}
	}
	if s.console != nil && s.publishErr == nil {
		if _, err := s.console.Publish(s.ctx, connect.NewRequest(&pb.PublishRequest{CommandId: s.id, Offset: s.offset, Data: p})); err != nil {
			s.publishErr = err
		} else {
			s.offset += uint64(len(p))
		}
	}
	// Never fail the command because a viewer or the disk failed.
	return len(p), nil
}

func (s *sink) finish(r *pb.CommandReport) {
	if s.console == nil {
		return
	}
	cause := ""
	if s.publishErr != nil {
		cause = s.publishErr.Error()
	}
	ctx, cancel := context.WithTimeout(context.WithoutCancel(s.ctx), 5*time.Second)
	defer cancel()
	if _, err := s.console.FinishPipe(ctx, connect.NewRequest(&pb.FinishPipeRequest{CommandId: s.id, CaptureError: cause, Report: r})); err != nil && s.publishErr == nil {
		s.publishErr = err
	}
}

func readEndpoint(path string) (endpoint, error) {
	if path == "" {
		return endpoint{}, errors.New("no Runner endpoint (--endpoint or LAZYBOX_V2_ENDPOINT)")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return endpoint{}, err
	}
	var address endpoint
	if err = json.Unmarshal(data, &address); err != nil {
		return endpoint{}, err
	}
	return address, nil
}

func newHTTPClient(token string) *http.Client { return &http.Client{Transport: authTransport{token}} }

// outputFile is private to the user: it holds unredacted output. The Runner
// names a directory under its state, deleted with the session; when that is
// unset or not writable (Codex's workspace-write sandbox allows only the
// workspace and the temp directories), the file goes to the temp directory,
// under the session's id so the Runner can still delete it with the session.
func outputFile(dir, session, name string) (*os.File, string, error) {
	pattern := safeName(filepath.Base(name)) + "-*.log"
	create := func(dir string) (*os.File, error) {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return nil, err
		}
		return os.CreateTemp(dir, pattern)
	}
	if dir != "" {
		if f, err := create(dir); err == nil {
			return f, f.Name(), nil
		}
	}
	fallback := filepath.Join(os.TempDir(), "lazybox-output")
	if session != "" && safeName(session) == session {
		fallback = filepath.Join(fallback, session)
	}
	f, err := create(fallback)
	if err != nil {
		return nil, "", err
	}
	return f, f.Name(), nil
}

func safeName(s string) string {
	s = strings.Map(func(r rune) rune {
		if r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '-' || r == '_' {
			return r
		}
		return '-'
	}, s)
	if s == "" {
		return "command"
	}
	return shorten(s, 40)
}

func shorten(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n-1] + "…"
}

func size(n int64) string {
	switch {
	case n >= 1<<20:
		return fmt.Sprintf("%.1f MiB", float64(n)/(1<<20))
	case n >= 1<<10:
		return fmt.Sprintf("%.1f KiB", float64(n)/(1<<10))
	}
	return fmt.Sprintf("%d B", n)
}
