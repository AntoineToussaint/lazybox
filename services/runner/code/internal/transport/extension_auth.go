package transport

import (
	"slices"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// ExtensionCredentials verifies the credential the app's bridge holds for
// one open extension panel (EXTENSIONS.md). runner.Bench implements it; a
// verifier without it admits no extension.
type ExtensionCredentials interface {
	// ExtensionScope returns the extension and approved grants of a live
	// extension credential, or an error for a forged or revoked one.
	ExtensionScope(token string) (extension string, grants []string, err error)
}

func authenticateExtension(agents AgentCredentials, token string) (principal, bool) {
	verifier, ok := agents.(ExtensionCredentials)
	if !ok || !strings.HasPrefix(token, power.ExtensionCredentialPrefix) {
		return principal{}, false
	}
	id, grants, err := verifier.ExtensionScope(token)
	if err != nil || id == "" {
		return principal{}, false
	}
	return principal{extension: id, grants: grants}, true
}

// extensionRule admits one RPC to an extension holding grant, for requests
// allow accepts (nil: any request of the RPC).
type extensionRule struct {
	grant string
	allow func(who principal, req any) bool
}

// extensionRPCs is everything an extension credential may call, by grant.
// Reading is what the bridge's read methods need: the conversation list
// (read:sessions, and read:attention, which the bridge narrows to what needs
// the person), a conversation's commands without their output, missions, and
// usage. Acting is three things: showing a conversation in the window,
// running one action of the registry the person approved by name (and only
// one marked safe), and sending a message to a conversation. Nothing else —
// a conversation's messages, files, terminals, settings, subscriptions,
// devices, the config files, approvals, starting or stopping work — and no
// RPC added later.
var extensionRPCs = map[string][]extensionRule{
	pb.RunnerService_ListHistory_FullMethodName:     {{grant: power.GrantReadSessions}, {grant: power.GrantReadAttention}},
	pb.RunnerService_ListCollections_FullMethodName: {{grant: power.GrantReadWorkspaces}},
	pb.RunnerService_ListCommands_FullMethodName: {{grant: power.GrantReadCommands, allow: func(_ principal, req any) bool {
		r, ok := req.(*pb.ListCommandsRequest)
		return ok && r.SessionId != ""
	}}},
	pb.RunnerService_GetWorkspaceRuntime_FullMethodName: {{grant: power.GrantReadRuntime, allow: func(_ principal, req any) bool {
		r, ok := req.(*pb.WorkspaceRuntimeRequest)
		return ok && r.CollectionId != ""
	}}},
	pb.RunnerService_ListLinks_FullMethodName: {{grant: power.GrantReadPulls, allow: func(_ principal, req any) bool {
		r, ok := req.(*pb.ListLinksRequest)
		return ok && r.SessionId == "" && !r.Focused && (r.All || r.CollectionId != "")
	}}},
	pb.RunnerService_ListArtifacts_FullMethodName: {{grant: power.GrantReadArtifacts, allow: func(_ principal, req any) bool {
		r, ok := req.(*pb.ListArtifactsRequest)
		return ok && r.CollectionId != "" && !r.IncludeRetired
	}}},
	pb.RunnerService_ListReleaseWatch_FullMethodName: {{grant: power.GrantReadReleases}},
	pb.RunnerService_ListMailbox_FullMethodName:      {{grant: power.GrantReadMailbox}},
	pb.RunnerService_ListMissions_FullMethodName:     {{grant: power.GrantReadMissions}},
	pb.RunnerService_GetTelemetry_FullMethodName:     {{grant: power.GrantReadUsage}},
	pb.RunnerService_Apply_FullMethodName: {{grant: power.GrantSendMessages, allow: func(_ principal, req any) bool {
		r, ok := req.(*pb.ApplyRequest)
		if !ok {
			return false
		}
		_, message := r.Operation.(*pb.ApplyRequest_Message)
		return message
	}}},
	pb.RunnerService_PostUiRequest_FullMethodName: {
		// Show a conversation or a workspace: no file, no panel, no screenshot.
		{grant: power.GrantShowConversations, allow: func(_ principal, req any) bool {
			r, ok := req.(*pb.UiRequest)
			return ok && r.Kind == power.UIOpen && (r.SessionId != "" || r.CollectionId != "") && r.File == "" && r.Panel == "" && r.Line == 0
		}},
		// Run one approved action; PostUiRequest also refuses one not marked safe.
		{allow: func(who principal, req any) bool {
			r, ok := req.(*pb.UiRequest)
			return ok && r.Kind == power.UIDo && r.ActionId != "" && slices.Contains(who.grants, power.GrantDoPrefix+r.ActionId)
		}},
	},
	// Only its own requests' answers (Poster, WaitUiRequest).
	pb.RunnerService_WaitUiRequest_FullMethodName: {{allow: func(who principal, _ any) bool {
		return slices.ContainsFunc(who.grants, func(g string) bool {
			return g == power.GrantShowConversations || strings.HasPrefix(g, power.GrantDoPrefix)
		})
	}}},
}

// authorizeExtension admits an extension credential to exactly the RPCs its
// approved grants name (extensionRPCs), with no stream.
func authorizeExtension(who principal, method string, req any) (principal, error) {
	denied := status.Error(codes.PermissionDenied, "this extension was not given that; its grants are approved in Settings → Extensions")
	if req == nil {
		return principal{}, denied
	}
	for _, rule := range extensionRPCs[method] {
		if rule.grant != "" && !slices.Contains(who.grants, rule.grant) {
			continue
		}
		if rule.allow == nil || rule.allow(who, req) {
			return who, nil
		}
	}
	return principal{}, denied
}

// poster is who posts a window request: "" for the owner, a hook's run, or
// an extension.
func (who principal) poster() string {
	if who.extension != "" {
		return "extension:" + who.extension
	}
	return who.hook
}
