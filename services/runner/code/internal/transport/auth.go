package transport

import (
	"context"
	"crypto/subtle"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
)

// AgentCredentials verifies the scoped credentials the Runner hands out
// (runner.Bench implements it): one per execution, given to its agent, and one
// per workspace terminal, given to the person's shell. Neither is the owner's
// bearer, so nothing an agent runs, and nothing started in a terminal, can
// approve a request, answer a question, send a message or change a setting
// with it (see agentRPCs for what that does not stop).
type AgentCredentials interface {
	// AgentSession returns the execution a scoped credential was issued to,
	// or an error for a forged or revoked one.
	AgentSession(ctx context.Context, token string) (string, error)
	// CommandSession returns the execution that owns a command or pipe.
	CommandSession(commandID string) (string, error)
	// TerminalScope returns the terminal a terminal credential was issued to
	// and its workspace, or an error for a forged or revoked one.
	TerminalScope(token string) (terminal, workspace string, err error)
	// CommandTerminal returns the terminal that registered a workspace run.
	CommandTerminal(commandID string) (string, error)
	// SessionWorkspace returns the workspace an execution works in, or "".
	SessionWorkspace(session string) (string, error)
	// RepositoryRequestSession returns the execution that asked for a repository.
	RepositoryRequestSession(request string) (string, error)
	// AskSession returns the execution that asked a question of the other
	// agent of its review.
	AskSession(ask string) (string, error)
}

// agentRPCs is everything an agent credential may call: what `lazybox run`
// and `lazybox log` need to publish one console, only for its own execution;
// what `lazybox artifact` needs to list, read and publish the artifacts of
// its own execution's workspace, and retire the ones agents published there; what `lazybox workspace` needs to list
// that workspace's repositories and ask the person to add one (never to add
// it); and what `lazybox review` needs for a reviewer to file and list its
// own review's findings (the Runner derives the review, its workspace and the
// author from the execution, and refuses one that reviews nothing); and what
// `lazybox link` and `lazybox items` need to link a pull request to its own
// workspace and list that workspace's links (never to unlink, react or merge).
// Every other RPC, including any added later, is the owner's.
//
// This allowlist prevents accidents, not a hostile agent. It holds for an
// agent that uses the credential it was given; an agent runs as the same OS
// user as the Runner and can read the Runner state (the owner's
// endpoint.json, agent-credential.key, the saved API key), so one that sets
// out to can act as the owner. SECURITY.md ("What an agent can reach") says
// what remains and the design that closes it.
var agentRPCs = map[string]func(agents AgentCredentials, who principal, req any) bool{
	// An agent reads and publishes only its own conversation's delivery
	// outcome (never a task lookup), and records a review verdict only as the
	// reviewer of its own review (runner.ErrNotReviewer otherwise).
	pb.RunnerService_GetDeliveryOutcome_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.GetDeliveryOutcomeRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session && r.MissionId == "" && r.TaskId == ""
	},
	pb.RunnerService_PublishDeliveryOutcome_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.PublishDeliveryOutcomeRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session && r.MissionId == "" && r.TaskId == ""
	},
	pb.RunnerService_RecordReviewCriterion_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RecordReviewCriterionRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	// An agent binds a spec only to its own conversation.
	pb.RunnerService_BindDelivery_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.BindDeliveryRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_RegisterPipe_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RegisterPipeRequest)
		return ok && r.SessionId == who.session && r.CollectionId == "" && r.ShareWith == ""
	},
	pb.RunnerService_Publish_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.PublishRequest)
		return ok && ownsCommand(agents, who.session, r.CommandId)
	},
	pb.RunnerService_FinishPipe_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.FinishPipeRequest)
		return ok && ownsCommand(agents, who.session, r.CommandId)
	},
	// A `lazybox run` follows its own pipe (stop, background); `lazybox ls`
	// and `lazybox stop` see and stop the execution's own commands only.
	pb.RunnerService_FollowPipe_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.FollowPipeRequest)
		return ok && ownsCommand(agents, who.session, r.CommandId)
	},
	pb.RunnerService_CancelCommand_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.CommandRequest)
		return ok && ownsCommand(agents, who.session, r.CommandId)
	},
	pb.RunnerService_ListCommands_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ListCommandsRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	// Reading is open across workspaces: an agent reads and lists any of the
	// person's artifacts (one workspace builds on another's plan or review).
	// Writing stays in its own workspace (PutArtifact, RetireArtifact).
	pb.RunnerService_ListArtifacts_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		_, ok := req.(*pb.ListArtifactsRequest)
		return ok
	},
	pb.RunnerService_GetArtifact_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		_, ok := req.(*pb.GetArtifactRequest)
		return ok
	},
	pb.RunnerService_PutArtifact_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.PutArtifactRequest)
		return ok && inOwnWorkspace(agents, who.session, r.CollectionId)
	},
	// Retiring, never restoring: the Runner admits only an artifact an agent
	// published (artifacts.go, retirableBy), and the person can restore it.
	pb.RunnerService_RetireArtifact_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RetireArtifactRequest)
		return ok && !r.Restore && inOwnWorkspace(agents, who.session, r.CollectionId)
	},
	pb.RunnerService_RenameCollection_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RenameCollectionRequest)
		return ok && inOwnWorkspace(agents, who.session, r.CollectionId)
	},
	pb.RunnerService_ListWorkspaceRepositories_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ListWorkspaceRepositoriesRequest)
		return ok && inOwnWorkspace(agents, who.session, r.CollectionId)
	},
	// The request is recorded on the asking execution's own conversation and
	// workspace, which the Runner derives from the execution, never the request.
	pb.RunnerService_RequestRepository_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RequestRepositoryRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	// Its own workspace's links: the Runner derives the workspace and the
	// conversation from the execution, never the request.
	pb.RunnerService_ListLinks_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ListLinksRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session && r.CollectionId == "" && !r.All
	},
	pb.RunnerService_LinkItem_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.LinkItemRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session && r.CollectionId == ""
	},
	pb.RunnerService_RunReviewVerification_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RunReviewVerificationRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_AddReviewFinding_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.AddReviewFindingRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_ListReviewFindings_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ListReviewFindingsRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_SetReviewFindingState_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.SetReviewFindingStateRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	// A question between the author and the reviewer of one review: the
	// Runner admits only those two, in their workspace, and derives the
	// other side from the review, never the request.
	pb.RunnerService_AskAgent_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.AskAgentRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_WaitAgentAsk_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.WaitAgentAskRequest)
		if !ok || r.AskId == "" {
			return false
		}
		asker, err := agents.AskSession(r.AskId)
		return err == nil && asker == who.session
	},
	pb.RunnerService_WaitRepositoryRequest_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.WaitRepositoryRequestRequest)
		if !ok || r.RequestId == "" {
			return false
		}
		asker, err := agents.RepositoryRequestSession(r.RequestId)
		return err == nil && asker == who.session
	},
	// An agent asks the person to start a conversation (conversation_starts.go)
	// for its own execution, and waits for its own requests' answers; it
	// never answers one.
	pb.RunnerService_RequestConversationStart_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RequestConversationStartRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_WaitConversationStart_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.WaitConversationStartRequest)
		starts, can := agents.(ConversationStartCredentials)
		if !ok || !can || r.RequestId == "" {
			return false
		}
		asker, err := starts.ConversationStartSession(r.RequestId)
		return err == nil && asker == who.session
	},
	// The Orchestrator verbs (agents.go), for the caller's own execution.
	// Starting an agent needs a grant in force, which only the person gives;
	// every target must be one of the children the caller started (or, for a
	// child, its parent, which it may message and wait on but never stop).
	pb.RunnerService_SpawnAgent_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.SpawnAgentRequest)
		o, can := orchestration(agents)
		return ok && can && r.SessionId != "" && r.SessionId == who.session && o.OrchestratorGrant(who.session)
	},
	pb.RunnerService_ListAgentLineage_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.AgentLineageRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_SendAgentMessage_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.SendAgentMessageRequest)
		o, can := orchestration(agents)
		return ok && can && r.SessionId != "" && r.SessionId == who.session && o.InLineage(who.session, r.To)
	},
	pb.RunnerService_WaitAgents_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.WaitAgentsRequest)
		o, can := orchestration(agents)
		if !ok || !can || r.SessionId == "" || r.SessionId != who.session {
			return false
		}
		for _, target := range r.Targets {
			if !o.InLineage(who.session, target) {
				return false
			}
		}
		return true
	},
	pb.RunnerService_StopChildAgent_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.StopChildAgentRequest)
		o, can := orchestration(agents)
		return ok && can && r.SessionId != "" && r.SessionId == who.session && r.Target != "parent" && o.InLineage(who.session, r.Target)
	},
	pb.RunnerService_ReportAgentTask_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ReportAgentTaskRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	// Leases are the caller's own task's (or, for an orchestrator, its
	// agents' tasks'): the Runner derives the task and the log checks it.
	pb.RunnerService_ClaimLease_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ClaimLeaseRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_ReleaseLease_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ReleaseLeaseRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	pb.RunnerService_ListLeases_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.LeasesRequest)
		return ok && r.SessionId != "" && r.SessionId == who.session
	},
	// A plan is proposed only by an orchestrator with a grant in force; only
	// the person decides it (UpdateMission, owner only).
	pb.RunnerService_ProposePlan_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.ProposePlanRequest)
		o, can := orchestration(agents)
		return ok && can && r.SessionId != "" && r.SessionId == who.session && o.OrchestratorGrant(who.session)
	},
}

// OrchestrationCredentials answers the Orchestrator verbs' checks
// (runner.Bench implements it): whether an execution's conversation holds a
// grant in force, and whether a target is in its lineage. A verifier without
// it admits none of those verbs.
type OrchestrationCredentials interface {
	OrchestratorGrant(session string) bool
	InLineage(session, target string) bool
}

func orchestration(agents AgentCredentials) (OrchestrationCredentials, bool) {
	o, ok := agents.(OrchestrationCredentials)
	return o, ok
}

// inOwnWorkspace admits a workspace only when it is the one the execution
// works in; an execution outside any workspace has no artifacts.
func inOwnWorkspace(agents AgentCredentials, session, workspace string) bool {
	if workspace == "" {
		return false
	}
	own, err := agents.SessionWorkspace(session)
	return err == nil && own == workspace
}

// terminalRPCs is everything a terminal credential may call: open a run in
// the terminal's own workspace (optionally shared with one of its
// conversations, which the person still has to send), and publish to and
// finish the runs that terminal opened. Nothing else, including any RPC added
// later.
var terminalRPCs = map[string]func(agents AgentCredentials, who principal, req any) bool{
	pb.RunnerService_RegisterPipe_FullMethodName: func(_ AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.RegisterPipeRequest)
		return ok && r.SessionId == "" && r.CollectionId == who.workspace
	},
	pb.RunnerService_Publish_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.PublishRequest)
		return ok && openedBy(agents, who.terminal, r.CommandId)
	},
	pb.RunnerService_FinishPipe_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.FinishPipeRequest)
		return ok && openedBy(agents, who.terminal, r.CommandId)
	},
	pb.RunnerService_FollowPipe_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.FollowPipeRequest)
		return ok && openedBy(agents, who.terminal, r.CommandId)
	},
	pb.RunnerService_CancelCommand_FullMethodName: func(agents AgentCredentials, who principal, req any) bool {
		r, ok := req.(*pb.CommandRequest)
		return ok && openedBy(agents, who.terminal, r.CommandId)
	},
}

func ownsCommand(agents AgentCredentials, session, command string) bool {
	owner, err := agents.CommandSession(command)
	return err == nil && owner == session
}

func openedBy(agents AgentCredentials, terminal, command string) bool {
	opener, err := agents.CommandTerminal(command)
	return err == nil && terminal != "" && opener == terminal
}

// principal is who a bearer belongs to: the owner, one execution's agent, or
// one workspace terminal.
type principal struct {
	owner               bool
	session             string
	terminal, workspace string
	// hook is a running hook's run id, and grants what its file granted it
	// (hook_auth.go).
	hook   string
	grants []string
	// extension is an open extension panel's extension id, and grants what
	// the person approved for it (extension_auth.go).
	extension string
}

type principalKey struct{}

// callerTerminal is the terminal whose credential made this call, or "".
func callerTerminal(ctx context.Context) string {
	who, _ := ctx.Value(principalKey{}).(principal)
	return who.terminal
}

// authenticate resolves the bearer to the owner, one execution or one
// terminal. Neither the token nor anything derived from it is ever put in an
// error.
func authenticate(ctx context.Context, owner string, agents AgentCredentials) (principal, error) {
	unauthenticated := status.Error(codes.Unauthenticated, "local test bench bearer required")
	md, _ := metadata.FromIncomingContext(ctx)
	values := md.Get("authorization")
	if len(values) != 1 {
		return principal{}, unauthenticated
	}
	if owner != "" && subtle.ConstantTimeCompare([]byte(values[0]), []byte("Bearer "+owner)) == 1 {
		return principal{owner: true}, nil
	}
	token, ok := strings.CutPrefix(values[0], "Bearer ")
	if !ok || agents == nil {
		return principal{}, unauthenticated
	}
	if terminal, workspace, err := agents.TerminalScope(token); err == nil && terminal != "" && workspace != "" {
		return principal{terminal: terminal, workspace: workspace}, nil
	}
	if who, ok := authenticateHook(agents, token); ok {
		return who, nil
	}
	if who, ok := authenticateExtension(agents, token); ok {
		return who, nil
	}
	session, err := agents.AgentSession(ctx, token)
	if err != nil || session == "" {
		return principal{}, unauthenticated
	}
	return principal{session: session}, nil
}

// authorize admits the owner to everything, an execution's credential only to
// agentRPCs for that execution, and a terminal's credential only to
// terminalRPCs for its workspace. req is nil for a streaming RPC, which
// neither may call.
func authorize(ctx context.Context, owner string, agents AgentCredentials, method string, req any) (principal, error) {
	if strings.HasPrefix(method, "/grpc.health.v1.Health/") {
		return principal{}, nil
	}
	who, err := authenticate(ctx, owner, agents)
	if err != nil {
		return principal{}, err
	}
	if who.owner {
		return who, nil
	}
	if who.hook != "" {
		return authorizeHook(who, method, req)
	}
	if who.extension != "" {
		return authorizeExtension(who, method, req)
	}
	if who.terminal != "" {
		if allowed := terminalRPCs[method]; req != nil && allowed != nil && allowed(agents, who, req) {
			return who, nil
		}
		return principal{}, status.Error(codes.PermissionDenied, "a terminal credential may only publish command output to its own workspace and stop the runs it started")
	}
	if allowed := agentRPCs[method]; req != nil && allowed != nil && allowed(agents, who, req) {
		return who, nil
	}
	return principal{}, status.Error(codes.PermissionDenied, agentDenial(method))
}

// agentDenial is what an agent is told when its credential may not make a
// call: one line, what was refused and the one next step. What an agent may
// do at all is agentRPCs (above), `lazybox --help` and RESOURCE_OWNERSHIP.md,
// not a paragraph in every refusal.
func agentDenial(method string) string {
	name := method[strings.LastIndex(method, "/")+1:]
	switch method {
	case pb.RunnerService_SpawnAgent_FullMethodName, pb.RunnerService_ProposePlan_FullMethodName:
		return "starting another agent needs the person's OK: ask them with `lazybox conversation new --title … --brief …` (one click for them)"
	case pb.RunnerService_SendAgentMessage_FullMethodName, pb.RunnerService_WaitAgents_FullMethodName, pb.RunnerService_StopChildAgent_FullMethodName:
		return "you reach only the agents you started, and your own parent: `lazybox agents ls` lists them"
	}
	if agentRPCs[method] != nil {
		return "refused: an agent's " + name + " reaches only its own conversation, commands and workspace"
	}
	return "refused: " + name + " is the person's to do, not an agent's; ask them in your reply"
}

// LocalAuthority protects the experimental Runner's execution authority on
// every protocol. Codefly's Connect listener invokes these same interceptors.
// owner is the bearer in the owner's endpoint file; agents verifies scoped
// execution and terminal credentials, and nil accepts the owner alone.
func LocalAuthority(owner string, agents AgentCredentials) []grpc.ServerOption {
	return []grpc.ServerOption{
		grpc.ChainUnaryInterceptor(func(ctx context.Context, req any, info *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (any, error) {
			who, err := authorize(ctx, owner, agents, info.FullMethod, req)
			if err != nil {
				return nil, err
			}
			return handler(context.WithValue(ctx, principalKey{}, who), req)
		}),
		grpc.ChainStreamInterceptor(func(srv any, stream grpc.ServerStream, info *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
			who, err := authorize(stream.Context(), owner, agents, info.FullMethod, nil)
			if err != nil {
				return err
			}
			if who.hook == "" {
				return handler(srv, stream)
			}
			// A hook's stream ends with its run's credential (revokeHook).
			verifier, _ := agents.(HookCredentials)
			ctx, release, err := verifier.BindHook(stream.Context(), who.hook)
			if err != nil {
				return status.Error(codes.Unauthenticated, "this hook's run ended; its credential is no longer valid")
			}
			defer release()
			return boundStream(ctx, runner.ErrHookCredentialEnded, handler(srv, &contextStream{ServerStream: stream, ctx: context.WithValue(ctx, principalKey{}, who)}))
		}),
	}
}
