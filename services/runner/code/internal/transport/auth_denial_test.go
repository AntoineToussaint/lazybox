package transport

import (
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// An agent's refusal is one line with the next step, never the paragraph of
// everything an agent credential may do: an agent that hit one in the field
// read a wall of text and could not tell what to do instead.
func TestAgentDenialIsOneActionableLine(t *testing.T) {
	for method, want := range map[string]string{
		pb.RunnerService_SpawnAgent_FullMethodName:         "lazybox conversation new",
		pb.RunnerService_ProposePlan_FullMethodName:        "lazybox conversation new",
		pb.RunnerService_SendAgentMessage_FullMethodName:   "lazybox agents ls",
		pb.RunnerService_PutArtifact_FullMethodName:        "its own conversation",
		pb.RunnerService_Apply_FullMethodName:              "Apply is the person's",
		pb.RunnerService_UpdateMission_FullMethodName:      "UpdateMission is the person's",
		pb.RunnerService_DeleteConversation_FullMethodName: "ask them in your reply",
	} {
		got := agentDenial(method)
		if !strings.Contains(got, want) || strings.Contains(got, "\n") || len(got) > 160 {
			t.Errorf("%s: %q", method, got)
		}
	}
}
