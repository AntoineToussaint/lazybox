package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// olderFormula lays out an older Lazybox installed by Homebrew's `lazybox`
// formula under home: the Cellar binary, the bin link Homebrew puts first on
// PATH, and a stand-in brew that records its arguments in brew.args and, for
// `uninstall lazybox`, removes the formula as Homebrew would. The real brew
// never runs.
func olderFormula(t *testing.T, home string) (bin, lazybox string) {
	t.Helper()
	cellar := filepath.Join(home, "homebrew", "Cellar", "lazybox", "0.1.21", "bin")
	bin = filepath.Join(home, "homebrew", "bin")
	for _, dir := range []string{cellar, bin} {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	writeScript(t, filepath.Join(cellar, "lazybox"), "echo lazybox 0.1.21")
	lazybox = filepath.Join(bin, "lazybox")
	if err := os.Symlink(filepath.Join(cellar, "lazybox"), lazybox); err != nil {
		t.Fatal(err)
	}
	writeScript(t, filepath.Join(bin, "brew"), `here=$(dirname "$0")
echo "$@" >> "$here/brew.args"
if [ "$1 $2" = "uninstall lazybox" ]; then
  rm -f "$here/lazybox"; rm -rf "$here/../Cellar/lazybox"
  echo "Uninstalling $here/../Cellar/lazybox/0.1.21... (3 files, 9MB)"
fi`)
	return bin, lazybox
}

func writeScript(t *testing.T, path, body string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("#!/bin/sh\n"+body+"\n"), 0o755); err != nil {
		t.Fatal(err)
	}
}

// realPath is path with its links resolved: the temporary directory is
// under /var, a link to /private/var, and brew is found through the Cellar.
func realPath(t *testing.T, path string) string {
	t.Helper()
	real, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return real
}

// brewRan is what the stand-in brew was asked to do, one call per line.
func brewRan(brew string) string {
	data, _ := os.ReadFile(filepath.Join(brew, "brew.args"))
	return string(data)
}

// The app runs `lazybox link` with launchd's PATH, which has neither
// Homebrew nor ~/.local/bin. What a terminal runs is decided by the login
// shell's PATH, and there an older Lazybox from Homebrew comes first: link
// and doctor say so, with the fix, and link makes only `lazybox`.
func TestLinkAndDoctorJudgeThePathByTheLoginShellAndNameTheOlderLazybox(t *testing.T) {
	r := newRelease(t, "0.1.0")
	brew, older := olderFormula(t, r.home)
	local := filepath.Join(r.home, ".local", "bin")
	r.path = "/usr/bin:/bin:/usr/sbin:/sbin"
	r.shell = brew + ":" + local + ":/usr/bin:/bin"

	got := r.run("", false, "link", "--dir", local, "--shell", "fish", "--json")
	if got.code != 0 {
		t.Fatalf("%+v", got)
	}
	var res linkResult
	if err := json.Unmarshal([]byte(got.stdout), &res); err != nil {
		t.Fatalf("%v: %s", err, got.stdout)
	}
	if !res.OnPath {
		t.Errorf("%s is on the login shell's PATH, but link says it is not: %+v", local, res)
	}
	problem := "Another `lazybox` (an older Lazybox, installed by Homebrew, at " + older + ") comes first on your PATH, so `lazybox` runs it."
	fix := "`brew uninstall lazybox` (removes the older Lazybox), or put " + local + " before " + brew + " in your shell's PATH."
	s := res.Shadow
	if s == nil || s.Path != older || s.Kind != shadowHomebrew || s.Problem != problem || s.Fix != fix || s.RemoveCommand != "brew uninstall lazybox" || strings.Join(s.Remove, " ") != realPath(t, filepath.Join(brew, "brew"))+" uninstall lazybox" {
		t.Errorf("the older Lazybox shadows the link, but: %+v", s)
	}
	hints := strings.Join(res.Hints, "\n")
	if !strings.Contains(hints, problem+" Fix: "+fix) {
		t.Errorf("hints lack the problem and its fix:\n%s", hints)
	}
	for _, never := range []string{"not on your PATH", "lazybox-v2", "V1", "V2"} {
		if strings.Contains(hints, never) {
			t.Errorf("hints say %q:\n%s", never, hints)
		}
	}
	if entries, _ := os.ReadDir(local); len(entries) != 1 || entries[0].Name() != "lazybox" {
		t.Errorf("link made more than `lazybox`: %v", entries)
	}
	// Linking where the formula's own `lazybox` is (Homebrew's bin on an
	// Intel Mac is /usr/local/bin) replaces nothing and names the fix.
	if got := r.run("", false, "link", "--dir", brew); got.code == 0 || !strings.Contains(got.err.Error(), "is an older Lazybox, installed by Homebrew: remove it with `brew uninstall lazybox`") {
		t.Errorf("linking over the formula: %+v", got)
	}
	if brewRan(brew) != "" {
		t.Fatalf("link ran brew: %s", brewRan(brew))
	}

	got = r.run("", false, "doctor", "--json")
	status := doctorChecks(t, got)
	if cli := status["cli"]; cli.Status != checkWarn || cli.Detail != problem || cli.Fix != fix {
		t.Errorf("cli: %+v", cli)
	}
	if status["shell"].Status != checkOK {
		t.Errorf("shell: %+v", status["shell"])
	}

	// With the older one gone, `lazybox` is this release.
	_ = os.Remove(older)
	if k := doctorChecks(t, r.run("", false, "doctor", "--json"))["cli"]; k.Status != checkOK || k.Detail != filepath.Join(local, "lazybox") {
		t.Errorf("cli without the older Lazybox: %+v", k)
	}
}

// `lazybox link --remove-older` runs `brew uninstall lazybox` only for an
// older Lazybox installed by Homebrew, only once confirmed, and says what
// `lazybox` runs afterwards.
func TestRemoveOlderRunsBrewUninstallOnlyOnceConfirmed(t *testing.T) {
	r := newRelease(t, "0.1.0")
	brew, older := olderFormula(t, r.home)
	local := filepath.Join(r.home, ".local", "bin")
	r.shell = brew + ":" + local + ":/usr/bin:/bin"
	if got := r.run("", false, "link", "--dir", local, "--no-completions"); got.code != 0 {
		t.Fatalf("%+v", got)
	}

	// Not confirmed: nothing runs.
	if got := r.run("", false, "link", "--remove-older"); got.code != exitUsage || !strings.Contains(got.err.Error(), "--yes") {
		t.Errorf("no terminal, no --yes: %+v", got)
	}
	if got := r.run("n\n", true, "link", "--remove-older"); got.code == 0 || !strings.Contains(got.stdout, realPath(t, filepath.Join(brew, "brew"))+" uninstall lazybox") {
		t.Errorf("declined: %+v", got)
	}
	if got := r.run("", false, "link", "--remove-older", "--dry-run"); got.code != 0 || !strings.Contains(got.stdout, "Would run: "+realPath(t, filepath.Join(brew, "brew"))+" uninstall lazybox") {
		t.Errorf("dry run: %+v", got)
	}
	if ran := brewRan(brew); ran != "" {
		t.Fatalf("brew ran without confirmation: %s", ran)
	}

	got := r.run("", false, "link", "--remove-older", "--yes", "--json")
	if got.code != 0 {
		t.Fatalf("%+v", got)
	}
	var res removeOlderResult
	if err := json.Unmarshal([]byte(got.stdout), &res); err != nil {
		t.Fatalf("%v: %s", err, got.stdout)
	}
	if brewRan(brew) != "uninstall lazybox\n" {
		t.Errorf("brew ran %q", brewRan(brew))
	}
	if !res.Removed || res.Command != "brew uninstall lazybox" || !strings.Contains(res.Output, "Uninstalling") || !res.Ours || res.Runs != filepath.Join(local, "lazybox") || res.Shadow != nil {
		t.Errorf("%+v", res)
	}
	if _, err := os.Lstat(older); err == nil {
		t.Errorf("%s is still there", older)
	}
	// Nothing left to remove.
	if got = r.run("", false, "link", "--remove-older", "--yes"); got.code != 0 || !strings.Contains(got.stdout, "Nothing to remove") {
		t.Errorf("again: %+v", got)
	}

	// A brew that fails is reported with its output, and nothing is claimed.
	brew2, _ := olderFormula(t, filepath.Join(r.home, "again"))
	writeScript(t, filepath.Join(brew2, "brew"), `echo "Error: Refusing to uninstall" >&2; exit 1`)
	r.shell = brew2 + ":" + local + ":/usr/bin:/bin"
	got = r.run("", false, "link", "--remove-older", "--yes", "--json")
	res = removeOlderResult{}
	_ = json.Unmarshal([]byte(got.stdout), &res)
	if got.code != exitFailure || res.Removed || !strings.Contains(res.Output, "Refusing") || !strings.Contains(res.Error, "brew uninstall lazybox") || res.Shadow == nil {
		t.Errorf("failed brew: %+v %+v", got, res)
	}
}

// What comes first is told apart by where it lives, and, when that does not
// say, by asking it for --version, bounded in time. Only an older Lazybox
// from Homebrew gets a removal; a shim is never run, and gets the order fix.
func TestIdentifyShadowTellsAnOlderLazyboxFromAShimOrAnotherProgram(t *testing.T) {
	defer func(d time.Duration) { versionProbeTimeout = d }(versionProbeTimeout)
	versionProbeTimeout = 300 * time.Millisecond
	home := realPath(t, t.TempDir())
	dir := filepath.Join(home, ".local", "bin")
	cli := "/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox"

	// A mise shim: a link to mise in mise's shims directory. Running it
	// would start mise, so it is identified by its path alone.
	ran := filepath.Join(home, "mise-ran")
	mise := filepath.Join(home, "mise", "bin", "mise")
	writeScript(t, mise, "touch "+ran+"; echo lazybox 0.1.21")
	shims := filepath.Join(home, ".local", "share", "mise", "shims")
	_ = os.MkdirAll(shims, 0o755)
	shim := filepath.Join(shims, "lazybox")
	if err := os.Symlink(mise, shim); err != nil {
		t.Fatal(err)
	}
	// An older Lazybox from cargo install, a program that is not Lazybox,
	// one that never answers, and another copy of the app.
	cargo := filepath.Join(home, ".cargo", "bin", "lazybox")
	writeScript(t, cargo, "echo lazybox 0.1.19")
	other := filepath.Join(home, "other", "lazybox")
	writeScript(t, other, "echo 'a box of lazy things'")
	slow := filepath.Join(home, "slow", "lazybox")
	writeScript(t, slow, "sleep 30")
	copy := filepath.Join(home, "Old.app", "Contents", "Resources", "runtime", "bin", "lazybox")
	writeScript(t, copy, "echo 'lazybox 0.0.9 (Lazybox V2)'")
	// A formula whose brew is not where its Cellar says.
	brewless := filepath.Join(home, "brewless", "Cellar", "lazybox", "0.1.2", "bin", "lazybox")
	writeScript(t, brewless, "echo lazybox 0.1.2")

	for _, c := range []struct {
		found, kind string
		what, fix   []string
		remove      bool
	}{
		{shim, shadowShim, []string{"a mise shim, not Lazybox itself"}, []string{"Put " + dir + " before " + shims + " in your shell's PATH", "after the line that sets up mise", "stop mise providing a `lazybox`"}, false},
		{cargo, shadowOlder, []string{"an older Lazybox, version 0.1.19"}, []string{"Remove " + cargo + " (the older Lazybox), or put " + dir + " before"}, false},
		{other, shadowOther, []string{"a program that is not this Lazybox"}, []string{"Put " + dir + " before " + filepath.Dir(other), "or rename or remove " + other}, false},
		{slow, shadowOther, []string{"a program that is not this Lazybox"}, []string{"rename or remove " + slow}, false},
		{copy, shadowCopy, []string{"another copy of Lazybox, in " + filepath.Join(home, "Old.app")}, []string{"`" + cli + " link --dir " + filepath.Dir(copy) + "` points it at this copy"}, false},
		{brewless, shadowHomebrew, []string{"an older Lazybox, installed by Homebrew"}, []string{"`brew uninstall lazybox`"}, false},
	} {
		start := time.Now()
		s := identifyShadow(context.Background(), c.found, cli, dir, true)
		if took := time.Since(start); took > 3*time.Second {
			t.Errorf("%s: took %s", c.found, took)
		}
		if s.Kind != c.kind || (len(s.Remove) > 0) != c.remove || !strings.HasPrefix(s.Problem, "Another `lazybox` (") || !strings.HasSuffix(s.Problem, "at "+c.found+") comes first on your PATH, so `lazybox` runs it.") {
			t.Errorf("%s: %+v", c.found, s)
		}
		for _, w := range c.what {
			if !strings.Contains(s.What, w) {
				t.Errorf("%s: what %q lacks %q", c.found, s.What, w)
			}
		}
		for _, w := range c.fix {
			if !strings.Contains(s.Fix, w) {
				t.Errorf("%s: fix %q lacks %q", c.found, s.Fix, w)
			}
		}
	}
	if _, err := os.Stat(ran); err == nil {
		t.Error("identifying a shim ran it")
	}
	// A directory not on PATH yet is added before the other one.
	if s := identifyShadow(context.Background(), shim, cli, dir, false); !strings.HasPrefix(s.Fix, "Add "+dir+" to your shell's PATH, before "+shims) {
		t.Errorf("off PATH: %q", s.Fix)
	}
}

// A shim first on PATH: setup and doctor say how to fix the order, and
// --remove-older refuses without running anything.
func TestRemoveOlderNeverRemovesAShim(t *testing.T) {
	r := newRelease(t, "0.1.0")
	local := filepath.Join(r.home, ".local", "bin")
	shims := filepath.Join(r.home, ".asdf", "shims")
	writeScript(t, filepath.Join(shims, "lazybox"), "touch "+filepath.Join(r.home, "shim-ran"))
	r.shell = shims + ":" + local + ":/usr/bin:/bin"
	if got := r.run("", false, "link", "--dir", local, "--no-completions"); got.code != 0 {
		t.Fatalf("%+v", got)
	}
	k := doctorChecks(t, r.run("", false, "doctor", "--json"))["cli"]
	if k.Status != checkWarn || !strings.Contains(k.Detail, "an asdf shim") || !strings.HasPrefix(k.Fix, "Put "+local+" before "+shims) || strings.Contains(k.Fix, "brew") {
		t.Errorf("cli: %+v", k)
	}
	got := r.run("", false, "link", "--remove-older", "--yes")
	if got.code != exitFailure || !strings.Contains(got.err.Error(), "not an older Lazybox installed by Homebrew") {
		t.Errorf("%+v", got)
	}
	if _, err := os.Stat(filepath.Join(r.home, "shim-ran")); err == nil {
		t.Error("the shim ran")
	}
}

// An earlier build linked a second name, lazybox-v2, beside `lazybox`:
// linking again takes it away, and uninstall removes one it finds recorded.
func TestTheOldSecondNameIsCleanedUp(t *testing.T) {
	r := newRelease(t, "0.1.0")
	local := filepath.Join(r.home, ".local", "bin")
	_ = os.MkdirAll(local, 0o755)
	alias := filepath.Join(local, legacyAliasName)
	if err := os.Symlink(r.runtime.CLI(), alias); err != nil {
		t.Fatal(err)
	}
	if err := writeRecord(r.paths, linkRecord{Links: []string{filepath.Join(local, "lazybox"), alias}}); err != nil {
		t.Fatal(err)
	}
	got := r.run("", false, "link", "--dir", local, "--no-completions")
	if got.code != 0 || !strings.Contains(got.stdout, "Removed "+alias) {
		t.Fatalf("%+v", got)
	}
	if _, err := os.Lstat(alias); err == nil {
		t.Errorf("%s survived link", alias)
	}
	if record := readRecord(r.paths); len(record.Links) != 1 || record.Links[0] != filepath.Join(local, "lazybox") {
		t.Errorf("record: %+v", record)
	}

	// An upgrade straight to uninstall: the recorded second name goes too.
	if err := os.Symlink(r.runtime.CLI(), alias); err != nil {
		t.Fatal(err)
	}
	_ = writeRecord(r.paths, linkRecord{Links: []string{filepath.Join(local, "lazybox"), alias}})
	if got = r.run("", false, "uninstall"); got.code != 0 {
		t.Fatalf("%+v", got)
	}
	for _, gone := range []string{filepath.Join(local, "lazybox"), alias} {
		if _, err := os.Lstat(gone); err == nil {
			t.Errorf("%s survived uninstall", gone)
		}
	}
}

// A login shell that cannot be read is said, not dropped: link falls back to
// the PATH it was started with, doctor warns, and service install --json
// reports it for the setup window.
func TestAShellThatCannotBeReadIsReported(t *testing.T) {
	r := newRelease(t, "0.1.0")
	bin := filepath.Join(r.home, "bin")
	r.path = bin + ":/usr/bin"
	r.shellProblem = "could not read the login shell's PATH: signal: killed"

	got := r.run("", false, "link", "--dir", bin, "--shell", "fish", "--json")
	var res linkResult
	if err := json.Unmarshal([]byte(got.stdout), &res); err != nil {
		t.Fatalf("%v: %s", err, got.stdout)
	}
	if !res.OnPath || res.PathProblem != r.shellProblem {
		t.Errorf("link: %+v", res)
	}

	got = r.run("", false, "service", "install", "--json")
	var report map[string]any
	if err := json.Unmarshal([]byte(got.stdout), &report); err != nil {
		t.Fatalf("%v: %s %s", err, got.stdout, got.stderr)
	}
	if report["shellProblem"] != r.shellProblem || report["shell"] != "/bin/zsh" {
		t.Errorf("service install --json dropped the shell problem: %v", report)
	}

	k := doctorChecks(t, r.run("", false, "doctor", "--json"))["shell"]
	if k.Status != checkWarn || !strings.Contains(k.Detail, "signal: killed") || !strings.Contains(k.Fix, "lazybox service install") {
		t.Errorf("shell: %+v", k)
	}
}

// The service keeps the PATH read when it was installed; doctor names the
// directories the login shell has gained since.
func TestDoctorNamesWhatTheAgentsPathLacks(t *testing.T) {
	r := newRelease(t, "0.1.0")
	if got := r.run("", false, "service", "install", "--no-start"); got.code != 0 {
		t.Fatalf("%+v", got)
	}
	status := doctorChecks(t, r.run("", false, "doctor", "--json"))
	if status["service-path"].Status != checkOK {
		t.Errorf("service-path: %+v", status["service-path"])
	}
	r.shell = "/opt/mise/shims:" + r.shell
	k := doctorChecks(t, r.run("", false, "doctor", "--json"))["service-path"]
	if k.Status != checkWarn || !strings.Contains(k.Detail, "/opt/mise/shims") || !strings.Contains(k.Fix, "lazybox service install") {
		t.Errorf("service-path: %+v", k)
	}
}

func TestSignInChecksSayWhetherEachAgentCanStartWork(t *testing.T) {
	sub := func(label, state, detail string) *pb.SubscriptionAvailability {
		return &pb.SubscriptionAvailability{Label: label, State: state, Detail: detail}
	}
	for _, tc := range []struct {
		name   string
		p      *pb.ProviderStatus
		show   bool
		status string
		words  []string
	}{
		{"ready", &pb.ProviderStatus{Provider: "claude", Installed: true, State: "ready", Subscriptions: []*pb.SubscriptionAvailability{sub("Local CLI sign-in", "available", ""), sub("Work", "unverified", "")}}, true, checkOK, []string{"Local CLI sign-in", "Work (not checked yet)"}},
		{"signed out", &pb.ProviderStatus{Provider: "claude", Installed: true, State: "sign_in", Subscriptions: []*pb.SubscriptionAvailability{sub("Local CLI sign-in", "sign_in", "Needs you to sign in again")}}, true, checkWarn, []string{"not signed in", "Needs you to sign in again", "/login"}},
		{"no subscription", &pb.ProviderStatus{Provider: "codex", Installed: true, State: "no_subscription"}, true, checkWarn, []string{"not signed in", "codex login"}},
		{"limited", &pb.ProviderStatus{Provider: "codex", Installed: true, State: "limited", Subscriptions: []*pb.SubscriptionAvailability{sub("Pro", "limited", "Reached its limit")}}, true, checkWarn, []string{"at its limit", "Reached its limit"}},
		{"not installed", &pb.ProviderStatus{Provider: "codex", State: "not_installed"}, false, "", nil},
		{"an ACP agent", &pb.ProviderStatus{Provider: "acp:gemini", Installed: true, State: "no_subscription", Acp: &pb.AcpAgentInfo{Id: "gemini"}}, false, "", nil},
	} {
		k, show := signInCheck(tc.p)
		if show != tc.show {
			t.Errorf("%s: shown %v", tc.name, show)
			continue
		}
		if !show {
			continue
		}
		text := k.Detail + " " + k.Fix
		if k.Status != tc.status || !strings.HasPrefix(k.ID, "signin-") {
			t.Errorf("%s: %+v", tc.name, k)
		}
		for _, w := range tc.words {
			if !strings.Contains(text, w) {
				t.Errorf("%s: %q lacks %q", tc.name, text, w)
			}
		}
	}
}

func doctorChecks(t *testing.T, got result) map[string]check {
	t.Helper()
	var report struct {
		Checks []check `json:"checks"`
	}
	if err := json.Unmarshal([]byte(got.stdout), &report); err != nil {
		t.Fatalf("%v: %s %s", err, got.stdout, got.stderr)
	}
	status := map[string]check{}
	for _, k := range report.Checks {
		status[k.ID] = k
	}
	return status
}
