package testselect

import "testing"

func TestWillRunFollowsListRunAndSkip(t *testing.T) {
	names := []string{"TestRemoteRunnerOverSSHEndToEnd", "TestRemoteRunnerOverKubernetesExec"}
	for _, c := range []struct {
		name            string
		list, run, skip string
		want            bool
	}{
		{name: "everything", want: true},
		{name: "one of them", run: "^TestRemoteRunnerOverKubernetesExec$", want: true},
		{name: "a prefix of them", run: "TestRemote", want: true},
		{name: "an alternation with one of them", run: "^(TestVersionNamesLazyboxV2|TestRemoteRunnerOverSSHEndToEnd)$", want: true},
		{name: "make release-smoke", run: "^TestRelease", want: false},
		{name: "a helper child's own test", run: "^TestVersionNamesLazyboxV2$", want: false},
		{name: "-list", list: ".", want: false},
		{name: "all of them skipped", skip: "TestRemoteRunner", want: false},
		{name: "one skipped, one left", skip: "SSH", want: true},
		{name: "per-level -run is not read: build", run: "^TestRelease/sub", want: true},
		{name: "per-level -skip is not read: build", skip: "TestRemoteRunner/x", want: true},
		{name: "a pattern that does not compile: build", run: "(", want: true},
	} {
		if got := willRun(c.list, c.run, c.skip, names); got != c.want {
			t.Errorf("%s: willRun(list=%q, run=%q, skip=%q) = %v, want %v", c.name, c.list, c.run, c.skip, got, c.want)
		}
	}
}
