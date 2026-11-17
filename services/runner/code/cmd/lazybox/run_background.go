package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"regexp"
	"slices"
	"strings"
	"sync"
	"syscall"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/listenports"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/processguard"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/serverlike"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
	"github.com/codefly-dev/gortk"
)

// Servers and watchers never exit, so whatever waits on them hangs: the
// agent's turn, the message the person queued behind it. `lazybox run` runs
// every command under a process guard (its whole process group is one unit),
// and once the grace period is over a command that still runs and listens on
// a TCP port, or looks like a server or watcher (serverlike), goes on in the
// background: the same process keeps running and streaming to the console,
// and `lazybox run` returns at once with exit 0 and a short report. The
// person can also move a run there from Lazybox (BackgroundCommand), and
// `--background` does it as soon as the command has started.
//
// Moving hands the command to a follower: this binary again, in a session of
// its own, given the guard's lifetime and outcome pipes and the command's
// output pipes (processguard.Release / Adopt). The follower publishes the
// rest of the output, reports the ports, stops the command when asked
// (`lazybox stop ID`, Stop in Lazybox) and records its exit. If the follower
// dies, the guard's helper stops the command's group: nothing is orphaned.

const (
	defaultGrace = 20 * time.Second
	// settle is how long --background waits, so a command that fails at once
	// still reports its failure.
	settle = 2 * time.Second
	// stopGrace is how long a stopped command has between SIGTERM and SIGKILL.
	stopGrace = 5 * time.Second
	// followWait is how long one FollowPipe waits for the Runner's request.
	followWait = 25 * time.Second
	// portEvery is how often a long-running command's ports are looked at.
	portEvery = 2 * time.Second
)

// waitFlag is --wait (stay in the foreground until the command exits) or
// --wait=DURATION (the grace period before it may move to the background).
type waitFlag struct{ o *runOptions }

func (w *waitFlag) IsBoolFlag() bool { return true }
func (w *waitFlag) String() string   { return "" }
func (w *waitFlag) Set(value string) error {
	switch value {
	case "true":
		w.o.wait = true
		return nil
	case "false":
		w.o.wait = false
		return nil
	}
	d, err := time.ParseDuration(value)
	if err != nil || d <= 0 {
		return errors.New("--wait takes no value (wait for the exit) or a duration, such as --wait=2m")
	}
	w.o.grace = d
	return nil
}

// foreground is one `lazybox run` while its command runs in the foreground.
type foreground struct {
	ctx            context.Context
	c              console
	o              runOptions
	target         pipeTarget
	argv           []string
	line, title    string
	s              *sink
	stdout, stderr io.Writer
	started        time.Time

	guard        *processguard.Guard
	outR, errR   *os.File
	readers      sync.WaitGroup
	exited       chan outcome
	followCancel context.CancelFunc
	requests     chan *pb.FollowPipeResponse
}

type outcome struct {
	processguard.Outcome
	err error
}

// run starts the command and waits for it to exit, or to move to the
// background. moved reports the latter: the report is written and the command
// runs on. code is the command's exit status otherwise.
func (f *foreground) run() (code int, moved bool, err error) {
	outR, outW, err := os.Pipe()
	if err != nil {
		return 0, false, err
	}
	errR, errW, err := os.Pipe()
	if err != nil {
		outR.Close()
		outW.Close()
		return 0, false, err
	}
	f.outR, f.errR = outR, errR
	cmd := exec.Command(f.argv[0], f.argv[1:]...)
	// No stdin: a command waiting for input would hang the agent's turn.
	cmd.Stdout, cmd.Stderr = outW, errW
	guard, startErr := processguard.Start(cmd)
	outW.Close()
	errW.Close()
	if startErr != nil {
		outR.Close()
		errR.Close()
		fmt.Fprintf(f.s.stream(&f.s.err), "lazybox: could not start %s: %v\n", f.argv[0], startErr)
		return 127, false, nil
	}
	f.guard = guard
	f.read()
	f.exited = make(chan outcome, 1)
	go f.wait(guard)
	f.requests = make(chan *pb.FollowPipeResponse, 4)
	followCtx, cancel := context.WithCancel(context.WithoutCancel(f.ctx))
	f.followCancel = cancel
	defer cancel()
	if f.s.console != nil && f.s.id != "" {
		go f.follow(followCtx)
	}

	var grace <-chan time.Time
	switch {
	case f.o.background:
		grace = time.After(settle)
	case !f.o.wait:
		g := f.o.grace
		if g <= 0 {
			g = defaultGrace
		}
		grace = time.After(g)
	}
	var portTick <-chan time.Time
	var stopTimer <-chan time.Time
	interrupted := f.ctx.Done()
	for {
		select {
		case result := <-f.exited:
			f.readers.Wait()
			outR.Close()
			errR.Close()
			return exitStatus(result, f.argv[0], f.s), false, nil
		case <-interrupted:
			// The agent's call or the person's Ctrl-C ended: the command ends too.
			interrupted = nil
			_ = guard.Interrupt()
			stopTimer = time.After(stopGrace)
		case <-stopTimer:
			_ = guard.Stop()
		case request := <-f.requests:
			if request.Stop && stopTimer == nil {
				_ = guard.Interrupt()
				stopTimer = time.After(stopGrace)
			} else if request.Background {
				if f.moveToBackground("you moved it to the background from Lazybox", nil) {
					return 0, true, nil
				}
			}
		case <-grace:
			grace = nil
			ports, _ := listenports.Of(f.ctx, guard.PID())
			switch looks, why := serverlike.MatchArgv(f.argv); {
			case f.o.background:
				if f.moveToBackground("you asked for it (--background)", ports) {
					return 0, true, nil
				}
			case len(ports) > 0:
				if f.moveToBackground(fmt.Sprintf("it listens on port %s", portList(ports)), ports) {
					return 0, true, nil
				}
			case looks:
				if f.moveToBackground(fmt.Sprintf("it looks like a server or watcher (%s)", shorten(why, 60)), ports) {
					return 0, true, nil
				}
			default:
				// Maybe a long test run, maybe a server still starting: keep
				// waiting, and move it once it listens.
				t := time.NewTicker(portEvery)
				defer t.Stop()
				portTick = t.C
			}
		case <-portTick:
			if ports, _ := listenports.Of(f.ctx, guard.PID()); len(ports) > 0 {
				if f.moveToBackground(fmt.Sprintf("it listens on port %s", portList(ports)), ports) {
					return 0, true, nil
				}
			}
		}
	}
}

// wait passes on the command's outcome; a guard handed to a follower has none.
func (f *foreground) wait(g *processguard.Guard) {
	o, err := g.Wait()
	if errors.Is(err, processguard.ErrReleased) {
		return
	}
	f.exited <- outcome{o, err}
}

// read copies the command's output into the sink until the pipes close, or
// until a move to the background sets a read deadline.
func (f *foreground) read() {
	for _, pipe := range []struct {
		r *os.File
		b *buffer
	}{{f.outR, &f.s.out}, {f.errR, &f.s.err}} {
		f.readers.Add(1)
		go func() {
			defer f.readers.Done()
			w := f.s.stream(pipe.b)
			buf := make([]byte, 32<<10)
			for {
				n, err := pipe.r.Read(buf)
				if n > 0 {
					_, _ = w.Write(buf[:n])
				}
				if err != nil {
					return
				}
			}
		}()
	}
}

// follow tells the Runner this run can be stopped and moved, and passes on
// what it asks. A Runner that does not know FollowPipe ends it quietly.
func (f *foreground) follow(ctx context.Context) {
	client, ok := f.c.(interface {
		FollowPipe(context.Context, *connect.Request[pb.FollowPipeRequest]) (*connect.Response[pb.FollowPipeResponse], error)
	})
	if !ok {
		return
	}
	for ctx.Err() == nil {
		r, err := client.FollowPipe(ctx, connect.NewRequest(&pb.FollowPipeRequest{CommandId: f.s.id, Pid: int32(f.guard.PID()), WaitMs: uint32(followWait / time.Millisecond)}))
		if err != nil {
			if connect.CodeOf(err) == connect.CodeUnimplemented || ctx.Err() != nil {
				return
			}
			select {
			case <-ctx.Done():
				return
			case <-time.After(2 * time.Second):
			}
			continue
		}
		if r.Msg.Stop || r.Msg.Background {
			select {
			case f.requests <- r.Msg:
			case <-ctx.Done():
				return
			}
			if r.Msg.Stop {
				return
			}
		}
	}
}

// handoff is what the follower needs to carry the run on.
type handoff struct {
	Endpoint   endpoint `json:"endpoint"`
	Console    string   `json:"console,omitempty"`
	Offset     uint64   `json:"offset"`
	OutputPath string   `json:"outputPath,omitempty"`
	Bytes      int64    `json:"bytes"`
	PID        int      `json:"pid"`
	Line       string   `json:"line"`
	Started    int64    `json:"started"`
	Workspace  bool     `json:"workspace,omitempty"`
	ReportSize int      `json:"reportSize"`
	Ports      []uint32 `json:"ports,omitempty"`
	Report     string   `json:"report,omitempty"`
}

// followArgument runs this binary as the follower of a run in the background.
const followArgument = "--lazybox-run-follow-v1"

func init() {
	if len(os.Args) >= 2 && os.Args[1] == followArgument {
		os.Exit(followMain())
	}
}

// moveToBackground hands the command to a follower and writes the report.
// It returns false, with the command still in the foreground, when the
// command exited meanwhile or the follower could not start.
func (f *foreground) moveToBackground(why string, ports []uint32) bool {
	f.followCancel()
	// Stop reading here: the follower reads from where this left off.
	now := time.Now()
	_ = f.outR.SetReadDeadline(now)
	_ = f.errR.SetReadDeadline(now)
	f.readers.Wait()
	resume := func() bool {
		_ = f.outR.SetReadDeadline(time.Time{})
		_ = f.errR.SetReadDeadline(time.Time{})
		f.read()
		return false
	}
	live, result, err := f.guard.Release()
	if err != nil {
		return resume() // It exited: the outcome is on its way.
	}
	f.s.mu.Lock()
	h := handoff{Console: f.s.id, Offset: f.s.offset, OutputPath: f.s.path, Bytes: f.s.total, PID: f.guard.PID(), Line: f.line, Started: f.started.UnixMilli(), Workspace: f.target.workspace(), Ports: ports}
	if f.s.console != nil {
		h.Endpoint = f.o.endpoint
	}
	tail := append([]byte(nil), f.s.tail...)
	publishErr := f.s.publishErr
	f.s.mu.Unlock()
	report := backgroundReport(h, why, tail, publishErr)
	h.Report, h.ReportSize = report, len(report)
	if err = startFollower(h, live, result, f.outR, f.errR); err != nil {
		// Keep it here; it can still be stopped and waited for.
		f.guard = processguard.Adopt(live, result, h.PID)
		go f.wait(f.guard)
		fmt.Fprintf(f.stderr, "lazybox: could not move %q to the background (%v); waiting for it to exit\n", shorten(f.title, 60), err)
		return resume()
	}
	live.Close()
	result.Close()
	f.outR.Close()
	f.errR.Close()
	if f.target.workspace() {
		fmt.Fprint(f.stderr, report) // the person's terminal
	} else {
		_, _ = io.WriteString(f.stdout, report)
	}
	return true
}

// startFollower starts the follower in a session of its own (so the agent's
// tool, or the person's terminal, ending does not end it) and waits until it
// holds the descriptors.
func startFollower(h handoff, live, result, outR, errR *os.File) error {
	self, err := os.Executable()
	if err != nil {
		return err
	}
	handR, handW, err := os.Pipe()
	if err != nil {
		return err
	}
	defer handR.Close()
	ackR, ackW, err := os.Pipe()
	if err != nil {
		handW.Close()
		return err
	}
	defer ackR.Close()
	cmd := exec.Command(self, followArgument)
	cmd.ExtraFiles = []*os.File{handR, live, result, outR, errR, ackW}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	err = cmd.Start()
	ackW.Close()
	if err != nil {
		handW.Close()
		return err
	}
	go func() { _ = cmd.Wait() }() // reaped if it ends while this process lives
	data, _ := json.Marshal(h)
	_, err = handW.Write(data)
	handW.Close()
	if err != nil {
		_ = cmd.Process.Kill()
		return err
	}
	_ = ackR.SetReadDeadline(time.Now().Add(10 * time.Second))
	ack := make([]byte, 1)
	if n, err := ackR.Read(ack); n != 1 || ack[0] != 'k' {
		_ = cmd.Process.Kill()
		if err == nil {
			err = errors.New("no acknowledgement")
		}
		return fmt.Errorf("the background process did not start: %w", err)
	}
	return nil
}

// backgroundReport is what the agent (or the person) gets when a command goes
// on in the background: where it runs, what it listens on, its latest
// output, where the rest goes and how to stop it.
func backgroundReport(h handoff, why string, tail []byte, publishErr error) string {
	var b strings.Builder
	elapsed := time.Since(time.UnixMilli(h.Started)).Round(100 * time.Millisecond)
	where := "running in the background"
	if h.Console != "" {
		where += " as command " + h.Console
	}
	fmt.Fprintf(&b, "[lazybox] %s → %s after %s: %s\n", shorten(h.Line, 200), where, elapsed, why)
	if len(h.Ports) > 0 {
		urls := make([]string, len(h.Ports))
		for i, p := range h.Ports {
			urls[i] = fmt.Sprintf("http://localhost:%d", p)
		}
		fmt.Fprintf(&b, "[lazybox] listening on %s\n", strings.Join(urls, ", "))
	}
	if lines := latestLines(tail, 8); len(lines) > 0 {
		b.WriteString("[lazybox] latest output:\n")
		for _, line := range lines {
			fmt.Fprintf(&b, "  %s\n", line)
		}
	} else {
		b.WriteString("[lazybox] no output yet\n")
	}
	if h.OutputPath != "" {
		fmt.Fprintf(&b, "[lazybox] output so far (%s, still growing): %s\n", size(h.Bytes), h.OutputPath)
	}
	switch {
	case h.Console != "" && publishErr == nil:
		fmt.Fprintf(&b, "[lazybox] the output streams to the Lazybox console. Stop it: lazybox stop %s · your commands: lazybox ls\n", h.Console)
	case h.Console != "":
		fmt.Fprintf(&b, "[lazybox] not shown in the Lazybox console: %v. Stop it: lazybox stop %s\n", publishErr, h.Console)
	default:
		fmt.Fprintf(&b, "[lazybox] not in the Lazybox console (no Runner). Stop it: kill -TERM -%d\n", h.PID)
	}
	return b.String()
}

// latestLines is the end of the output as a person reads it: colour codes
// and secrets out (gortk), carriage-return redraws resolved, blank lines
// dropped, at most count lines of at most 200 characters.
func latestLines(tail []byte, count int) []string {
	if len(bytes.TrimSpace(tail)) == 0 {
		return nil
	}
	text := compressor.Compress(gortk.CommandFromArgs([]string{"lazybox-background"}, ansi.ReplaceAll(tail, nil), nil, 0)).Text
	var lines []string
	for _, line := range strings.Split(text, "\n") {
		if i := strings.LastIndex(strings.TrimRight(line, "\r"), "\r"); i >= 0 {
			line = line[i+1:]
		}
		line = strings.TrimRight(line, " \t\r")
		if strings.TrimSpace(line) == "" {
			continue
		}
		lines = append(lines, shorten(line, 200))
	}
	if len(lines) > count {
		lines = lines[len(lines)-count:]
	}
	return lines
}

// ansi matches terminal escape sequences: colours, cursor moves, titles.
var ansi = regexp.MustCompile(`\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]`)

func portList(ports []uint32) string {
	parts := make([]string, len(ports))
	for i, p := range ports {
		parts[i] = fmt.Sprint(p)
	}
	return strings.Join(parts, ", ")
}

// exitStatus is the command's exit status, as `lazybox run` returns it.
func exitStatus(r outcome, program string, s *sink) int {
	if !r.Started {
		fmt.Fprintf(s.stream(&s.err), "lazybox: could not start %s\n", program)
		return 127
	}
	if r.ExitCode < 0 { // killed by a signal
		return 1
	}
	return r.ExitCode
}

// ---- The follower ---------------------------------------------------------

// followMain carries a run on in the background: descriptors 3 to 8 are the
// handoff, the guard's lifetime and outcome pipes, the command's stdout and
// stderr, and the acknowledgement.
func followMain() int {
	files := make([]*os.File, 6)
	for i := range files {
		files[i] = os.NewFile(uintptr(3+i), "lazybox-run")
		if files[i] == nil {
			return 125
		}
	}
	data, err := io.ReadAll(io.LimitReader(files[0], 1<<20))
	files[0].Close()
	var h handoff
	if err != nil || json.Unmarshal(data, &h) != nil || h.PID <= 0 {
		return 125
	}
	guard := processguard.Adopt(files[1], files[2], h.PID)
	_, _ = files[5].Write([]byte{'k'})
	files[5].Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	(&follower{h: h, guard: guard, outR: files[3], errR: files[4]}).run(ctx)
	return 0
}

type follower struct {
	h          handoff
	guard      *processguard.Guard
	outR, errR *os.File
	client     genconnect.RunnerServiceClient
	pub        *publisher
	file       *os.File
	mu         sync.Mutex
	total      int64
}

func (f *follower) run(ctx context.Context) {
	if f.h.Endpoint.URL != "" && f.h.Console != "" {
		f.client = genconnect.NewRunnerServiceClient(newHTTPClient(f.h.Endpoint.Token), f.h.Endpoint.URL)
		f.pub = &publisher{client: f.client, id: f.h.Console, offset: f.h.Offset}
	}
	if f.h.OutputPath != "" {
		if file, err := os.OpenFile(f.h.OutputPath, os.O_WRONLY|os.O_APPEND, 0o600); err == nil {
			f.file = file
			defer file.Close()
		}
	}
	f.total = f.h.Bytes
	var readers sync.WaitGroup
	for _, r := range []*os.File{f.outR, f.errR} {
		readers.Add(1)
		go func() {
			defer readers.Done()
			buf := make([]byte, 32<<10)
			for {
				n, err := r.Read(buf)
				if n > 0 {
					f.write(buf[:n])
				}
				if err != nil {
					return
				}
			}
		}()
	}
	exited := make(chan outcome, 1)
	go func() {
		o, err := f.guard.Wait()
		exited <- outcome{o, err}
	}()
	followCtx, stopFollowing := context.WithCancel(ctx)
	stopped := make(chan struct{}, 1)
	if f.client != nil {
		go f.follow(followCtx, stopped)
	}
	var stopTimer <-chan time.Time
	var result outcome
	for done := false; !done; {
		select {
		case result = <-exited:
			done = true
		case <-stopped:
			_ = f.guard.Interrupt()
			stopTimer = time.After(stopGrace)
		case <-stopTimer:
			_ = f.guard.Stop()
		}
	}
	stopFollowing()
	// The group is gone; its pipes close a moment later.
	waited := make(chan struct{})
	go func() { readers.Wait(); close(waited) }()
	select {
	case <-waited:
	case <-time.After(2 * time.Second):
	}
	f.finish(result)
}

func (f *follower) write(p []byte) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.total += int64(len(p))
	if f.file != nil {
		if _, err := f.file.Write(p); err != nil {
			f.file = nil
		}
	}
	if f.pub != nil {
		f.pub.add(p)
	}
}

// follow keeps the Runner told that the run is in the background, with its
// ports, and passes on a stop. A Runner that restarts is asked again.
func (f *follower) follow(ctx context.Context, stopped chan<- struct{}) {
	ports, looked := f.h.Ports, time.Now()
	for ctx.Err() == nil {
		if time.Since(looked) >= 5*time.Second {
			if p, err := listenports.Of(ctx, f.h.PID); err == nil {
				ports = p
			}
			looked = time.Now()
		}
		wait := followWait
		if len(ports) == 0 && time.Since(time.UnixMilli(f.h.Started)) < 5*time.Minute {
			wait = 5 * time.Second // a server still starting gets its port reported soon
		}
		r, err := f.client.FollowPipe(ctx, connect.NewRequest(&pb.FollowPipeRequest{CommandId: f.h.Console, Background: true, Pid: int32(f.h.PID), Ports: ports, WaitMs: uint32(wait / time.Millisecond)}))
		if err != nil {
			if ctx.Err() != nil {
				return
			}
			if ended(err) {
				// Lazybox no longer follows it (its conversation was deleted,
				// the Runner closed): nobody could see or stop it, so it stops.
				select {
				case stopped <- struct{}{}:
				default:
				}
				return
			}
			select {
			case <-ctx.Done():
				return
			case <-time.After(2 * time.Second):
			}
			continue
		}
		f.pub.resync(r.Msg.Offset)
		f.pub.flush()
		if r.Msg.Stop {
			select {
			case stopped <- struct{}{}:
			default:
			}
			return
		}
	}
}

// ended reports a Runner that has ended the run's pipe, or no longer accepts
// its credential, as opposed to one that is restarting or unreachable.
func ended(err error) bool {
	switch connect.CodeOf(err) {
	case connect.CodeNotFound, connect.CodeFailedPrecondition, connect.CodePermissionDenied, connect.CodeUnauthenticated:
		return true
	}
	return false
}

// finish records the command's exit with the Runner, retrying through a
// Runner restart for a while.
func (f *follower) finish(r outcome) {
	if f.client == nil {
		return
	}
	code := r.ExitCode
	if !r.Started {
		code = 127
	}
	f.mu.Lock()
	total := f.total
	f.mu.Unlock()
	cause := ""
	if dropped := f.pub.droppedBytes(); dropped > 0 {
		cause = fmt.Sprintf("%s of output not shown while the Runner was unreachable", size(dropped))
	}
	request := &pb.FinishPipeRequest{CommandId: f.h.Console, CaptureError: cause, Signal: r.Signal, Report: &pb.CommandReport{ExitCode: int32(code), OutputBytes: uint64(total), AgentBytes: uint64(f.h.ReportSize), Filter: "background", OutputPath: f.h.OutputPath}}
	if f.h.Workspace {
		request.Report.AgentView = boundView(f.h.Report)
	}
	for deadline := time.Now().Add(2 * time.Minute); time.Now().Before(deadline); time.Sleep(2 * time.Second) {
		f.pub.flush()
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, err := f.client.FinishPipe(ctx, connect.NewRequest(request))
		cancel()
		if err == nil || connect.CodeOf(err) == connect.CodeNotFound || connect.CodeOf(err) == connect.CodePermissionDenied {
			return
		}
	}
}

// publisher sends a background run's output to the console, keeping what a
// restarting Runner could not take (up to a bound) to send once it is back.
type publisher struct {
	mu      sync.Mutex
	client  genconnect.RunnerServiceClient
	id      string
	offset  uint64
	pending []byte
	dropped int64
	noted   int64
}

const publisherBacklog = 8 << 20

func (p *publisher) add(data []byte) {
	if p == nil {
		return
	}
	p.mu.Lock()
	p.pending = append(p.pending, data...)
	if over := len(p.pending) - publisherBacklog; over > 0 {
		p.pending = slices.Clone(p.pending[over:])
		p.dropped += int64(over)
	}
	p.mu.Unlock()
	p.flush()
}

// resync follows the Runner's count of the output it holds: a publish whose
// reply was lost is not sent twice.
func (p *publisher) resync(held uint64) {
	if p == nil {
		return
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if held > p.offset {
		skip := min(int(held-p.offset), len(p.pending))
		p.pending = p.pending[skip:]
		p.offset = held
	}
}

func (p *publisher) flush() {
	if p == nil {
		return
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.dropped > p.noted {
		note := fmt.Appendf(nil, "\n[lazybox] %s of output not shown while the Runner was unreachable; the full-output file has it\n", size(p.dropped-p.noted))
		p.pending = append(note, p.pending...)
		p.noted = p.dropped
	}
	for len(p.pending) > 0 {
		n := min(len(p.pending), 32<<10)
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, err := p.client.Publish(ctx, connect.NewRequest(&pb.PublishRequest{CommandId: p.id, Offset: p.offset, Data: p.pending[:n]}))
		cancel()
		if err != nil {
			return // kept for the next flush
		}
		p.offset += uint64(n)
		p.pending = p.pending[n:]
	}
}

func (p *publisher) droppedBytes() int64 {
	if p == nil {
		return 0
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.dropped
}
