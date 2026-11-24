// Package testselect tells a TestMain whether a test that needs an expensive
// fixture (a compiled CLI, a packed runtime) will run in this test binary, so
// the fixture is built once before the tests when one of them is selected,
// and not at all for `-list`, a `-run` that selects none of them, or a test
// binary re-executed as a helper with its own `-run`.
//
// Building in TestMain keeps the compile out of every test's 30 s deadline
// (scripts/go-test-guard.mjs times tests, not TestMain); go test's -timeout
// bounds it with the rest of the binary.
package testselect

import (
	"flag"
	"regexp"
	"strings"
)

// WillRun reports whether go test will run at least one of the named
// top-level tests, from this binary's -test.list, -test.run and -test.skip.
// Call it from TestMain after flag.Parse. When the patterns are too intricate
// to read here (a "/" between levels), it answers yes: a fixture built in
// vain costs time, a missing one fails the tests that need it.
func WillRun(names ...string) bool {
	return willRun(value("test.list"), value("test.run"), value("test.skip"), names)
}

func willRun(list, run, skip string, names []string) bool {
	if list != "" {
		return false // -list prints test names and runs none
	}
	for _, name := range names {
		if matches(run, name, true) && !matches(skip, name, false) {
			return true
		}
	}
	return false
}

// matches is go test's match of one top-level test name against -run or
// -skip; unsure answers what it is told to (yes for -run, no for -skip).
func matches(pattern, name string, unsure bool) bool {
	if pattern == "" {
		return unsure // no -run runs everything; no -skip skips nothing
	}
	if strings.Contains(pattern, "/") {
		return unsure // per-level patterns: go test splits them, this does not
	}
	re, err := regexp.Compile(pattern)
	if err != nil {
		return unsure
	}
	return re.MatchString(name)
}

func value(name string) string {
	if f := flag.Lookup(name); f != nil {
		return f.Value.String()
	}
	return ""
}
