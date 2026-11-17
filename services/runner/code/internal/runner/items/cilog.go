package items

import (
	"regexp"
	"strconv"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/codefly-dev/gortk"
)

// A GitHub Actions job's log is every step's output, each line stamped with
// its time, each step opened by "##[group]Run <command>". The agent needs
// what it would have seen running the failing step itself: so the failing
// step is found (the one GitHub ended with "##[error]"), and its output goes
// through the very filter `lazybox run` uses, keyed by the step's command —
// `go test` output gets the go-test filter, as it would locally.

var (
	logStamp   = regexp.MustCompile(`^\x{feff}?\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z ?`)
	exitStatus = regexp.MustCompile(`^##\[error\]Process completed with exit code (\d+)\.?$`)
)

// JobView is the compact view of a failing job's log.
type JobView struct {
	// Step is the failing step's command, when the log names one.
	Step string
	// Exit is the step's exit status (-1 when the log does not say).
	Exit int
	// Text is the compact view; Filter the filter that made it.
	Text, Filter string
	// Lossy: lines were dropped; the full log is the file.
	Lossy bool
}

// viewLimit bounds one job's view in a message.
const viewLimit = 16 << 10

type step struct {
	command string
	header  bool
	lines   []string
	failed  bool
	exit    int
}

// CompactJobLog finds a job log's failing step and compacts its output.
func CompactJobLog(raw []byte) JobView {
	var steps []*step
	current := &step{exit: -1}
	steps = append(steps, current)
	for _, line := range strings.Split(strings.ReplaceAll(string(raw), "\r\n", "\n"), "\n") {
		line = logStamp.ReplaceAllString(line, "")
		switch {
		case strings.HasPrefix(line, "##[group]Run "):
			current = &step{command: strings.TrimSpace(strings.TrimPrefix(line, "##[group]Run ")), header: true, exit: -1}
			steps = append(steps, current)
			continue
		case line == "##[endgroup]":
			current.header = false
			continue
		case current.header:
			continue // the step's script and environment, echoed
		case strings.HasPrefix(line, "Post job cleanup."):
			current = &step{exit: -1}
			steps = append(steps, current)
			continue
		}
		if m := exitStatus.FindStringSubmatch(line); m != nil {
			current.failed = true
			current.exit, _ = strconv.Atoi(m[1])
			continue
		}
		if rest, ok := strings.CutPrefix(line, "##[error]"); ok {
			current.failed = true
			line = "error: " + rest
		} else if rest, ok := strings.CutPrefix(line, "##[warning]"); ok {
			line = "warning: " + rest
		} else if strings.HasPrefix(line, "##[") {
			continue // group, debug and command markers
		}
		current.lines = append(current.lines, line)
	}
	var failing *step
	for _, s := range steps {
		if s.failed {
			failing = s
		}
	}
	view := JobView{Exit: -1}
	var body []string
	command := "ci-log"
	if failing != nil {
		view.Step, view.Exit, body = failing.command, failing.exit, failing.lines
		if failing.command != "" {
			command = failing.command
		}
	} else {
		for _, s := range steps {
			body = append(body, s.lines...)
		}
	}
	exit := view.Exit
	if exit < 0 {
		exit = 1
	}
	text := strings.TrimSpace(strings.Join(body, "\n")) + "\n"
	result := commands.Compressor.Compress(gortk.CommandFromLine(command, []byte(text), nil, exit))
	view.Text, view.Filter, view.Lossy = strings.TrimRight(result.Text, "\n"), result.Filter, !result.Lossless()
	if len(view.Text) > viewLimit {
		// The end of a failure is what explains it.
		view.Text = "[lazybox] the start of this view was cut; the full log is in the file\n" + view.Text[len(view.Text)-viewLimit:]
		view.Lossy = true
	}
	return view
}

var actionsJob = regexp.MustCompile(`/actions/runs/\d+/job(?:s)?/(\d+)`)

// ActionsJobID is the GitHub Actions job behind a check, or 0 for another CI.
func ActionsJobID(c Check) int64 {
	if m := actionsJob.FindStringSubmatch(c.URL); m != nil {
		id, _ := strconv.ParseInt(m[1], 10, 64)
		return id
	}
	return 0
}
