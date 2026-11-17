package runner

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// The reboot gate (SESSION_HOSTS.md, "After a reboot"). A reboot is the one
// restart session hosts do not survive: it kills the Runner, every agent,
// command and terminal host, and the programs in them, all at once. What
// must come back is everything the Runner wrote down: the conversation,
// its turn checkpoints, its missions, the messages queued behind a turn, and
// the commands it ran with their output. The agent itself comes back through
// the provider's own session resume, with a notice that says what happened,
// and without asking the person to vouch that processes they cannot see are
// stopped: the Runner checked.
//
// The machine is not rebooted: every process a reboot would kill is killed
// with SIGKILL (Runner group, hosts, the programs' groups), then a new
// Runner starts on the same state, as launchd does at login.

// rebootFixture is what the fixture Runner set up before the "reboot".
type rebootFixture struct {
	Execution    string `json:"execution"`
	Conversation string `json:"conversation"`
	Finished     string `json:"finished"`
	Running      string `json:"running"`
	Mission      string `json:"mission"`
	Task         string `json:"task"`
}

// fixtureRebootRunner leaves a conversation as a person's day would before
// the Mac restarts: one finished turn (checkpointed), a second turn waiting
// for an approval, a message queued behind it, a finished command and a
// running one, and a mission whose task this conversation works on.
func fixtureRebootRunner() {
	fail := func(err error) { fmt.Println("error", err); os.Exit(2) }
	b := fixtureBench(nil, os.Getenv("RUNNER_STATE"))
	e, err := b.Open("claude-default", os.Getenv("RUNNER_REPO"), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		fail(err)
	}
	ctx := context.Background()
	until := func(what string, ok func() bool) {
		for end := time.Now().Add(20 * time.Second); !ok(); time.Sleep(10 * time.Millisecond) {
			if time.Now().After(end) {
				fail(fmt.Errorf("%s: %+v", what, e.Engine.Snapshot()))
			}
		}
	}
	pending := func() bool { _, ok := e.Engine.Snapshot().Pending["attempt-0:req-1"]; return ok }
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "first question"}); err != nil {
		fail(err)
	}
	until("first approval", pending)
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "approve-first", Kind: agent.Approve, RequestID: "attempt-0:req-1", Decision: "accept"}); err != nil {
		fail(err)
	}
	until("first turn", func() bool { s := e.Engine.Snapshot(); return !s.Busy && len(s.Pending) == 0 })
	until("first checkpoint", func() bool {
		r, err := b.CodeCheckpoints(e.ID)
		return err == nil && len(r.Turns) == 1 && r.Turns[0].After != nil
	})
	finished, err := b.Start(e, "build", []string{"/bin/sh", "-c", "echo build passed"})
	if err != nil {
		fail(err)
	}
	until("finished command", func() bool { return finished.Status().Done })
	running, err := b.Start(e, "serve", []string{"/bin/sh", "-c", "echo serving; exec /bin/sleep 300"})
	if err != nil {
		fail(err)
	}
	until("running command output", func() bool { data, _, _ := running.Read(ctx, 0, 0); return strings.Contains(string(data), "serving") })
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "second question"}); err != nil {
		fail(err)
	}
	until("second approval", pending)
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "third", Kind: agent.Message, Text: "queued question"}); err != nil {
		fail(err)
	}
	until("queued message", func() bool { return e.Engine.Snapshot().Queued == 1 })
	mission, err := b.Work.Create(work.Human, "mission", "Ship the release", "Everything comes back after a reboot")
	if err != nil {
		fail(err)
	}
	_, task, err := b.Work.AddTask(mission.ID, work.Human, "task", "", "Release notes", "Write them", nil)
	if err != nil {
		fail(err)
	}
	if _, _, err = b.Work.Assign(mission.ID, task, work.Human, "assign", work.AttemptPayload{Conversation: conversationOf(e), Execution: e.ID}); err != nil {
		fail(err)
	}
	data, _ := json.Marshal(rebootFixture{Execution: e.ID, Conversation: conversationOf(e), Finished: finished.Status().ID, Running: running.Status().ID, Mission: mission.ID, Task: task})
	fmt.Println(string(data))
	select {}
}

// reboot kills, as a reboot does, the Runner and every process a session
// host runs, and waits until none is left. Each host's record stays on disk,
// as it does across a real reboot.
func reboot(t *testing.T, runner *exec.Cmd, state string) {
	t.Helper()
	var pids []int
	for _, root := range []string{"hosts", "command-hosts", "terminal-hosts"} {
		root = filepath.Join(state, root)
		ids, _ := sessionhost.Hosts(root)
		for _, id := range ids {
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			status, err := sessionhost.Probe(ctx, root, id)
			cancel()
			if err == nil && status.AgentPID > 0 {
				pids = append(pids, status.AgentPID)
			}
			record, err := sessionhost.ReadRecord(filepath.Join(root, id))
			if err != nil {
				t.Fatalf("host %s has no record: %v", id, err)
			}
			pids = append(pids, record.PID)
		}
	}
	if len(pids) < 4 {
		t.Fatalf("expected an agent and a command, each with its host, before the reboot: %v", pids)
	}
	_ = syscall.Kill(-runner.Process.Pid, syscall.SIGKILL)
	_ = runner.Wait()
	for _, pid := range pids {
		// The program first leads its group (processguard); kill both.
		_ = syscall.Kill(-pid, syscall.SIGKILL)
		_ = syscall.Kill(pid, syscall.SIGKILL)
	}
	for _, pid := range pids {
		stoppedEventually(t, pid)
	}
}

func TestRebootBringsBackEverythingAndResumesTheAgent(t *testing.T) {
	state, repo := t.TempDir(), t.TempDir()
	cleanupHosts(t, state)
	for _, args := range [][]string{{"init", "-q", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-q", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
	}
	log := filepath.Join(t.TempDir(), "worker.log")
	t.Setenv("FIXTURE_WORKER_LOG", log)

	self, _ := os.Executable()
	first := exec.Command(self)
	first.Env = append(os.Environ(), "RUNNER_FIXTURE=reboot", "RUNNER_STATE="+state, "RUNNER_REPO="+repo)
	first.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	out, err := first.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err = first.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = syscall.Kill(-first.Process.Pid, syscall.SIGKILL) })
	line, err := bufio.NewReader(out).ReadString('\n')
	if err != nil || strings.HasPrefix(line, "error") {
		t.Fatalf("fixture Runner: %q %v", line, err)
	}
	var before rebootFixture
	if err = json.Unmarshal([]byte(line), &before); err != nil {
		t.Fatal(err)
	}
	reboot(t, first, state)

	// The Runner launchd starts at login.
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	b := fixtureBench(t, state)
	defer b.Close()
	if err = b.Adopt(ctx); err != nil {
		t.Fatalf("a reboot is not a failure to report: %v", err)
	}

	// The conversation: whole, ended, and continuable by sending a message.
	e, err := b.Get(before.Execution)
	if err != nil {
		t.Fatal(err)
	}
	snapshot := e.Engine.Snapshot()
	if !e.Archived || !snapshot.Closed || snapshot.Busy {
		t.Fatalf("the conversation came back as live: archived=%v %+v", e.Archived, snapshot)
	}
	if e.RecoveryRequired {
		t.Fatal("after a reboot the person is asked to confirm processes the Runner can see are gone")
	}
	if snapshot.Status != agent.Interrupted || snapshot.Reference != "fixture-ref" || !snapshot.Capabilities.Resume {
		t.Fatalf("not resumable from the provider's session: %+v", snapshot)
	}
	events := recorded(t, b, before.Execution)
	said := map[string]bool{}
	notice := ""
	for _, ev := range events {
		if ev.Type == "operation_accepted" {
			said[ev.Text] = true
		}
		if ev.Type == "message_delta" && strings.HasPrefix(ev.Text, "accept by ") {
			said["answer"] = true
		}
		if ev.Type == AgentLostEvent {
			notice = ev.Text
		}
	}
	for _, want := range []string{"first question", "answer", "second question", "queued question"} {
		if !said[want] {
			t.Errorf("transcript lost %q: %+v", want, events)
		}
	}
	if !strings.Contains(notice, "restarted") || !strings.Contains(notice, "send a message") {
		t.Fatalf("no notice says what happened and what to do: %q", notice)
	}
	// The message queued behind the interrupted turn is still waiting, and says so.
	if len(snapshot.QueuedMessages) != 1 || snapshot.QueuedMessages[0].Text != "queued question" {
		t.Fatalf("queued message lost: %+v", snapshot.QueuedMessages)
	}

	// Turn checkpoints.
	checkpoints, err := b.CodeCheckpoints(before.Execution)
	if err != nil || len(checkpoints.Turns) < 1 || checkpoints.Turns[0].After == nil {
		t.Fatalf("turn checkpoints lost: %+v %v", checkpoints, err)
	}

	// The mission and the task this conversation works on.
	mission, err := b.Work.Mission(before.Mission)
	if err != nil {
		t.Fatalf("mission lost: %v", err)
	}
	if mission.Title != "Ship the release" || len(mission.Tasks) != 1 {
		t.Fatalf("mission came back changed: %+v", mission)
	}

	// Command history: the finished command as it was, the running one ended
	// with an outcome the person can read, both with their output.
	commands := map[string]bool{}
	for _, c := range b.Commands.List(before.Execution) {
		commands[c.ID] = c.Done
	}
	if !commands[before.Finished] || !commands[before.Running] {
		t.Fatalf("command history lost: %+v", b.Commands.List(before.Execution))
	}
	for id, want := range map[string]string{before.Finished: "build passed", before.Running: "serving"} {
		c, err := b.Commands.Get(id)
		if err != nil {
			t.Fatal(err)
		}
		data, status, err := c.Read(ctx, 0, 0)
		if err != nil || !strings.Contains(string(data), want) {
			t.Fatalf("command %s output lost: %q %v", id, data, err)
		}
		if id == before.Running && !strings.Contains(status.Error, "outcome unknown") {
			t.Fatalf("an interrupted command does not say so: %+v", status)
		}
	}

	// Continue: the agent resumes its provider session, and the queued message
	// is delivered to it before anything the person sends next.
	next, err := b.Resume(ctx, before.Execution, false)
	if err != nil {
		t.Fatalf("resume after a reboot: %v", err)
	}
	if next.Previous != before.Execution {
		t.Fatalf("not a continuation: %+v", next)
	}
	for end := time.Now().Add(10 * time.Second); ; time.Sleep(20 * time.Millisecond) {
		data, _ := os.ReadFile(log)
		lines := string(data)
		if strings.Contains(lines, "open resume=fixture-ref") && strings.Contains(lines, "message queued question") {
			break
		}
		if time.Now().After(end) {
			t.Fatalf("the agent was not resumed with its session, or the queued message not delivered:\n%s", lines)
		}
	}
	if s := next.Engine.Snapshot(); s.Reference != "fixture-ref" {
		t.Fatalf("resumed into another provider session: %+v", s)
	}
	// The same conversation: checkpoints span its attempts, the mission keeps it.
	if conversationOf(next) != before.Conversation {
		t.Fatalf("the resumed attempt is another conversation: %s != %s", conversationOf(next), before.Conversation)
	}
	if _, err = os.Stat(b.attachmentPath(before.Execution)); !os.IsNotExist(err) {
		t.Fatalf("the predecessor's queue was kept after it was handed over: %v", err)
	}
	if again, _ := b.Get(before.Execution); len(again.Engine.Snapshot().QueuedMessages) != 0 {
		t.Fatal("a delivered message is still shown as queued on the predecessor")
	}
}

// The notice a conversation gets is written once: a later Runner start does
// not repeat it, and the conversation stays resumable.
func TestAgentLostNoticeIsRecordedOnce(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	first := fixtureBench(t, state)
	e, err := openAwaitingApproval(first)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, first, e.ID))
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	// The host and its agent die while no Runner runs.
	ids, _ := sessionhost.Hosts(filepath.Join(state, "hosts"))
	for _, id := range ids {
		record, _ := sessionhost.ReadRecord(filepath.Join(state, "hosts", id))
		_ = syscall.Kill(record.PID, syscall.SIGKILL)
	}
	_ = syscall.Kill(-pid, syscall.SIGKILL)
	stoppedEventually(t, pid)
	for range 2 {
		b := fixtureBench(t, state)
		if err = b.Adopt(context.Background()); err != nil {
			t.Fatal(err)
		}
		if err = b.Close(); err != nil {
			t.Fatal(err)
		}
	}
	b := fixtureBench(t, state)
	defer b.Close()
	if n := count(recorded(t, b, e.ID), AgentLostEvent); n != 1 {
		t.Fatalf("notice recorded %d times", n)
	}
	got, err := b.Get(e.ID)
	if err != nil || got.RecoveryRequired || !got.Engine.Snapshot().Closed {
		t.Fatalf("not resumable: %+v %v", got, err)
	}
}
