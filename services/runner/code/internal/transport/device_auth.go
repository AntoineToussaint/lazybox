package transport

import (
	"context"
	"errors"
	"net/http"
	"strings"

	"connectrpc.com/vanguard"
	"connectrpc.com/vanguard/vanguardgrpc"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"
)

// DeviceVerifier resolves a paired device's access credential
// (devices.Manager implements it).
type DeviceVerifier interface {
	Authenticate(token string) (devices.Device, error)
	// Bind authenticates a credential for a stream and returns a context
	// that ends, with cause devices.ErrCredentialEnded, at the credential's
	// expiry or when its device is revoked; release ends the binding.
	Bind(ctx context.Context, token string) (bound context.Context, release context.CancelFunc, d devices.Device, err error)
}

// deviceRule admits one RPC to a device holding grant, for requests allow
// accepts (nil accepts any request of the RPC).
type deviceRule struct {
	grant string
	allow func(req any) bool
}

// deviceRPCs is everything a paired device may call (REMOTE.md). Reading
// covers what a phone shows: the inbox, a conversation and its output, a
// workspace's changes. Responding is what the person does in a conversation:
// send, answer, approve or decline, interrupt. Starting runs a new agent
// process, so it is a separate grant. Nothing else — settings, subscriptions,
// GitHub, terminals, commands, files, deletion, processes, restart, devices,
// and any RPC added later — is reachable with a device credential.
var deviceRPCs = map[string]deviceRule{
	pb.RunnerService_Info_FullMethodName:                 {grant: devices.GrantRead},
	pb.RunnerService_ListHistory_FullMethodName:          {grant: devices.GrantRead},
	pb.RunnerService_GetSession_FullMethodName:           {grant: devices.GrantRead},
	pb.RunnerService_ListCollections_FullMethodName:      {grant: devices.GrantRead},
	pb.RunnerService_GetCollection_FullMethodName:        {grant: devices.GrantRead},
	pb.RunnerService_ListWorkspaceChanges_FullMethodName: {grant: devices.GrantRead},
	pb.RunnerService_ReadWorkspaceDiff_FullMethodName:    {grant: devices.GrantRead},
	pb.RunnerService_ListCommands_FullMethodName:         {grant: devices.GrantRead},
	pb.RunnerService_ReadCommand_FullMethodName:          {grant: devices.GrantRead},
	// Images a sent message carried are part of reading its conversation; the
	// attachment store serves only retained message images, at most 20 MiB.
	pb.RunnerService_ReadAttachment_FullMethodName: {grant: devices.GrantRead},
	// The latest turns of a conversation, and earlier pages: what WatchEvents
	// replays, in fewer, larger reads.
	pb.RunnerService_ReadSessionTail_FullMethodName: {grant: devices.GrantRead},
	pb.RunnerService_Apply_FullMethodName: {grant: devices.GrantRespond, allow: func(req any) bool {
		r, ok := req.(*pb.ApplyRequest)
		if !ok {
			return false
		}
		// Changing a running agent's permission mode (to yolo, say) or its
		// model is a setting, not a response.
		switch r.Operation.(type) {
		case *pb.ApplyRequest_Message, *pb.ApplyRequest_Steer, *pb.ApplyRequest_Interrupt, *pb.ApplyRequest_Answer, *pb.ApplyRequest_Approval, *pb.ApplyRequest_DeliverQueued:
			return true
		}
		return false
	}},
	pb.RunnerService_OpenSession_FullMethodName: {grant: devices.GrantStart, allow: func(req any) bool {
		r, ok := req.(*pb.OpenSessionRequest)
		// Only in an existing workspace, with the Runner's own defaults for
		// everything that decides what the agent may do or what it bills: a
		// phone does not pick a directory, resume an arbitrary provider thread,
		// raise an agent's permissions or leave plan-by-default (workflow),
		// choose the gateway, instructions or behavior profile, or rotate onto
		// other accounts. The model and its effort are the phone's to choose.
		if !ok || r.CollectionId == "" || r.Cwd != "" || r.ResumeReference != "" || r.EmptyWorkspace || r.Fork != nil {
			return false
		}
		return deviceStartOptions(r.Options)
	}},
	// Continuing an ended conversation, never with processes_stopped: that is
	// the owner's statement, after an unclean shutdown, that the previous
	// agent and its commands are gone — which a phone (or a hook) cannot
	// know, and which would start a second agent beside one still running.
	pb.RunnerService_ResumeSession_FullMethodName: {grant: devices.GrantStart, allow: func(req any) bool {
		r, ok := req.(*pb.ResumeSessionRequest)
		return ok && !r.ProcessesStopped
	}},
	// A phone's own push notifications (push.go): reading what needs the person
	// includes being told, so a read-only phone may subscribe. Each RPC acts on
	// the calling device only; the service refuses another device's id.
	pb.RunnerService_GetPushSettings_FullMethodName: {grant: devices.GrantRead},
	pb.RunnerService_SetPushSettings_FullMethodName: {grant: devices.GrantRead},
	pb.RunnerService_SendTestPush_FullMethodName:    {grant: devices.GrantRead},
	// Pinning a conversation is the person's arrangement of their own lists,
	// shared with the computer (audit R1/R7p): a phone that may respond may pin.
	// It changes no execution state. Archiving and deleting stay the owner's.
	pb.RunnerService_PinConversation_FullMethodName: {grant: devices.GrantRespond},
	// Tracked pull requests: reading them is reading what needs the person;
	// sending a failure or a review to the conversation that delivered it is
	// a response, like a message. Merging, linking and unlinking stay the owner's.
	pb.RunnerService_ListLinks_FullMethodName: {grant: devices.GrantRead, allow: func(req any) bool {
		r, ok := req.(*pb.ListLinksRequest)
		return ok && r.SessionId == ""
	}},
	pb.RunnerService_SendPullRequestFailures_FullMethodName: {grant: devices.GrantRespond},
	pb.RunnerService_ImportPullRequestReview_FullMethodName: {grant: devices.GrantRespond},
	// An agent's request to start a conversation waits for the person like an
	// approval: a phone that may respond starts it as asked, or declines it.
	// Choosing another agent, model or workspace is the computer's (the card's Edit…).
	pb.RunnerService_AnswerConversationStart_FullMethodName: {grant: devices.GrantRespond, allow: func(req any) bool {
		r, ok := req.(*pb.AnswerConversationStartRequest)
		return ok && (r.Decision == "start" || r.Decision == "decline") && r.ProfileId == "" && r.Model == "" && r.Workspace == ""
	}},
}

// deviceStartOptions admits the options a phone may start a conversation
// with: none but the model and its effort. It clears exactly those two from a
// copy and requires what remains to be empty, so an option added to
// RunnerOptions later is refused until it is decided here.
func deviceStartOptions(o *pb.RunnerOptions) bool {
	if o == nil {
		return true
	}
	rest := proto.Clone(o).(*pb.RunnerOptions)
	rest.Model, rest.Effort = "", ""
	return proto.Size(rest) == 0
}

// deviceStreams are the server streams a device may follow.
var deviceStreams = map[string]string{
	pb.RunnerService_WatchEvents_FullMethodName: devices.GrantRead,
	pb.RunnerService_WatchOutput_FullMethodName: devices.GrantRead,
}

type deviceKey struct{}

// callerDevice is the paired device that made this call, or nil.
func callerDevice(ctx context.Context) *devices.Device {
	d, _ := ctx.Value(deviceKey{}).(*devices.Device)
	return d
}

// authorizeDevice admits a device credential, and only a device credential,
// to the RPCs its grants allow. The owner's bearer and agent or terminal
// credentials are refused: the device gateway is not another way in for them.
// req is nil for a stream.
func authorizeDevice(ctx context.Context, verifier DeviceVerifier, method string, req any) (*devices.Device, error) {
	unauthenticated := status.Error(codes.Unauthenticated, "a paired device's credential is required; sign in again from the phone")
	md, _ := metadata.FromIncomingContext(ctx)
	values := md.Get("authorization")
	if len(values) != 1 || verifier == nil {
		return nil, unauthenticated
	}
	token, ok := strings.CutPrefix(values[0], "Bearer ")
	if !ok || !strings.HasPrefix(token, devices.TokenPrefix) {
		return nil, unauthenticated
	}
	d, err := verifier.Authenticate(token)
	if err != nil {
		return nil, unauthenticated
	}
	denied := status.Error(codes.PermissionDenied, "this device is not allowed to do that; change what it may do by pairing it again from Settings → Devices")
	if req == nil {
		if grant, ok := deviceStreams[method]; ok && d.Has(grant) {
			return &d, nil
		}
		return nil, denied
	}
	rule, ok := deviceRPCs[method]
	if !ok || !d.Has(rule.grant) || (rule.allow != nil && !rule.allow(req)) {
		return nil, denied
	}
	return &d, nil
}

// DeviceAuthority protects the device gateway's RPC endpoint. It is a
// different server from the owner's loopback listener, with this interceptor
// alone, so a credential valid on one is refused on the other.
func DeviceAuthority(verifier DeviceVerifier) []grpc.ServerOption {
	return []grpc.ServerOption{
		grpc.ChainUnaryInterceptor(func(ctx context.Context, req any, info *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (any, error) {
			d, err := authorizeDevice(ctx, verifier, info.FullMethod, req)
			if err != nil {
				return nil, err
			}
			return handler(context.WithValue(ctx, deviceKey{}, d), req)
		}),
		grpc.ChainStreamInterceptor(func(srv any, stream grpc.ServerStream, info *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
			if _, err := authorizeDevice(stream.Context(), verifier, info.FullMethod, nil); err != nil {
				return err
			}
			// A stream lasts no longer than its credential: it ends at the
			// credential's expiry (the phone renews and reconnects) and at once
			// when the device is revoked.
			token, _ := strings.CutPrefix(metadata.ValueFromIncomingContext(stream.Context(), "authorization")[0], "Bearer ")
			ctx, release, d, err := verifier.Bind(stream.Context(), token)
			if err != nil {
				return status.Error(codes.Unauthenticated, "a paired device's credential is required; sign in again from the phone")
			}
			defer release()
			return boundStream(ctx, devices.ErrCredentialEnded, handler(srv, &contextStream{ServerStream: stream, ctx: context.WithValue(ctx, deviceKey{}, &d)}))
		}),
	}
}

// deviceInfo is what Info tells a device: conversations and limits, and each
// account by name only. Usage, email addresses, sign-in flows and gateways
// stay on the owner's screen.
func deviceInfo(full *pb.InfoResponse) *pb.InfoResponse {
	out := &pb.InfoResponse{Sessions: full.Sessions, DefaultModel: full.DefaultModel, AttachmentLimits: full.AttachmentLimits, InstanceId: full.InstanceId, RunnerId: full.RunnerId}
	for _, p := range full.Profiles {
		out.Profiles = append(out.Profiles, &pb.Profile{Id: p.Id, Provider: p.Provider, Label: p.Label, Managed: p.Managed, Isolation: p.Isolation})
	}
	return out
}

// DeviceHandler serves the Runner's API to paired devices over Connect, on a
// gRPC server of its own whose only authority is DeviceAuthority. stop ends
// that server.
func DeviceHandler(service pb.RunnerServiceServer, verifier DeviceVerifier, maxMessageBytes int) (handler http.Handler, stop func(), err error) {
	server := grpc.NewServer(append([]grpc.ServerOption{grpc.MaxRecvMsgSize(maxMessageBytes)}, DeviceAuthority(verifier)...)...)
	pb.RegisterRunnerServiceServer(server, service)
	// The same omitted-default ProtoJSON profile as the owner's Connect listener.
	codec := vanguard.WithCodec(func(res vanguard.TypeResolver) vanguard.Codec {
		c := vanguard.NewJSONCodec(res)
		c.MarshalOptions.EmitUnpopulated = false
		c.MarshalOptions.EmitDefaultValues = false
		return c
	})
	transcoder, err := vanguardgrpc.NewTranscoder(server, codec, vanguard.WithDefaultServiceOptions(vanguard.WithTargetCodecs(vanguard.CodecProto), vanguard.WithMaxMessageBufferBytes(uint32(maxMessageBytes))))
	if err != nil {
		return nil, nil, err
	}
	return transcoder, server.Stop, nil
}

// contextStream is a server stream whose handler sees ctx.
type contextStream struct {
	grpc.ServerStream
	ctx context.Context
}

func (s *contextStream) Context() context.Context { return s.ctx }

// boundStream is what a stream whose context is bound to a credential
// returns: Unauthenticated when the credential ended under it (cause ended),
// so the client renews and reconnects or learns it was revoked, and the
// handler's own result otherwise.
func boundStream(ctx context.Context, ended error, result error) error {
	if errors.Is(context.Cause(ctx), ended) {
		return status.Error(codes.Unauthenticated, "this credential ended (expired or revoked); sign in again")
	}
	return result
}
