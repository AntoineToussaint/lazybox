package transport

import (
	"context"
	"slices"
	"strings"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// HookCredentials verifies the credential a running hook holds when the
// user's actions file grants it one (POWER.md). runner.Bench implements it;
// a verifier without it admits no hook.
type HookCredentials interface {
	// HookScope returns the run and grants of a live hook credential, or an
	// error for a forged one or one whose run has ended.
	HookScope(token string) (run string, grants []string, err error)
	// BindHook returns a context that ends, with cause
	// runner.ErrHookCredentialEnded, when the run's credential ends, so a
	// stream the hook opened does not outlive it; release ends the binding.
	BindHook(ctx context.Context, run string) (bound context.Context, release context.CancelFunc, err error)
}

// hookTokenPrefix marks a hook credential (runner.hookTokenPrefix).
const hookTokenPrefix = "lbx-hook."

func authenticateHook(agents AgentCredentials, token string) (principal, bool) {
	verifier, ok := agents.(HookCredentials)
	if !ok || !strings.HasPrefix(token, hookTokenPrefix) {
		return principal{}, false
	}
	run, grants, err := verifier.HookScope(token)
	if err != nil || run == "" {
		return principal{}, false
	}
	return principal{hook: run, grants: grants}, true
}

// hookExcluded are device RPCs a hook does not get: Info carries account and
// sign-in details a phone receives only redacted; push settings and test
// pushes are a phone's own, and a hook is no phone (a hook could otherwise
// send any phone test notifications as the owner does).
var hookExcluded = map[string]bool{
	pb.RunnerService_Info_FullMethodName:            true,
	pb.RunnerService_GetPushSettings_FullMethodName: true,
	pb.RunnerService_SetPushSettings_FullMethodName: true,
	pb.RunnerService_SendTestPush_FullMethodName:    true,
}

// hookWindowRPCs: with respond, a hook may ask the open window to run an
// action of the app (`lazybox do`) and wait for its own requests' answers.
// PostUiRequest refuses a hook any request but do, and any action not marked
// safe (power.go); WaitUiRequest answers only its own requests.
var hookWindowRPCs = map[string]func(req any) bool{
	pb.RunnerService_PostUiRequest_FullMethodName: func(req any) bool {
		r, ok := req.(*pb.UiRequest)
		return ok && r.Kind == "do"
	},
	pb.RunnerService_WaitUiRequest_FullMethodName: func(any) bool { return true },
}

// authorizeHook admits a hook credential to exactly what a paired phone with
// the same grants may call (deviceRPCs, deviceStreams), less Info, plus the
// window's safe actions (hookWindowRPCs). Nothing else — settings, commands,
// files, terminals, processes, devices, the actions and config files, and the
// other window requests — and no RPC added later.
func authorizeHook(who principal, method string, req any) (principal, error) {
	denied := status.Error(codes.PermissionDenied, "this hook's credential does not allow that; grant it in the actions file (read, respond, start)")
	if hookExcluded[method] {
		return principal{}, denied
	}
	if allow, ok := hookWindowRPCs[method]; ok {
		if req != nil && slices.Contains(who.grants, "respond") && allow(req) {
			return who, nil
		}
		return principal{}, denied
	}
	if req == nil {
		if grant, ok := deviceStreams[method]; ok && slices.Contains(who.grants, grant) {
			return who, nil
		}
		return principal{}, denied
	}
	rule, ok := deviceRPCs[method]
	if !ok || !slices.Contains(who.grants, rule.grant) || (rule.allow != nil && !rule.allow(req)) {
		return principal{}, denied
	}
	return who, nil
}
