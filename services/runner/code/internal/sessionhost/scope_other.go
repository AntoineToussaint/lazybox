//go:build !linux

package sessionhost

import "os/exec"

// ScopeStatus says whether hosts start in systemd scopes of their own, and
// why not. Only Linux has them (scope_linux.go); elsewhere a host's own
// session is what keeps it out of reach of the Runner's service manager.
type ScopeStatus struct {
	Scoped bool
	Reason string
}

// disableScopes is set by tests that check what happens without a scope.
var disableScopes bool

// Scopes reports that hosts do not use systemd scopes here.
func Scopes() ScopeStatus { return ScopeStatus{Reason: "systemd scopes are Linux's"} }

// ScopeUnit is the transient scope a host with this id would run in on Linux.
func ScopeUnit(id string) string { return "lazybox-host-" + id + ".scope" }

func inOwnScope(*exec.Cmd, string, string) bool { return false }

func scopeFailed(string) {}
