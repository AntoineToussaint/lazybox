package runner

import (
	"context"
	"os"
	"path/filepath"
	"sync"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Each child emits a valid fixture. Only replaying the same completed command
// twice across the producer/consumer boundary distinguishes append from upsert.
func TestCombinedReconnectAndPromotion(t *testing.T) {
	ctx := context.Background()
	sources := []collections.Repository{}
	spec := collections.Spec{Name: "Base"}
	for _, name := range []string{"runner", "client"} {
		src := upstreamOn(t, "main")
		sources = append(sources, collections.Repository{ID: name, Name: name, Path: src})
		spec.Members = append(spec.Members, collections.MemberSpec{Name: name, RepositoryID: name, BaseRef: gitIn(t, src, "rev-parse", "HEAD")})
	}
	b := freshBench(t, sources...)
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	base, err := b.Collections.Create(ctx, "base", spec)
	must(err)
	baseline, err := b.planBaseline(ctx, base.ID)
	must(err)
	m, err := b.Work.Create(work.Human, "", "Reconnect", "")
	must(err)
	actor := work.Actor{Kind: work.ActorAgent, Role: "orchestrator", Conversation: "parent"}
	_, grant, err := b.Work.IssueGrant(m.ID, work.Human, "", work.GrantPayload{Holder: actor.Conversation, Ceiling: work.Ceiling{Permission: "ask", Profiles: []string{"codex-default"}, MaxChildren: 2, Depth: 1}})
	must(err)
	_, plan, err := b.Work.ProposePlan(m.ID, actor, "", work.PlanPayload{Grant: grant, Contract: &work.PlanContract{Workspace: base.ID, Digest: "spec-digest"}, Baseline: baseline, Tasks: []work.PlanTask{{Key: "runner", Title: "Producer", Workspace: "pinned"}, {Key: "client", Title: "Consumer", Workspace: "pinned"}}})
	must(err)
	s, err := b.Work.DecidePlan(m.ID, plan, work.Human, "", true, 0, 2, "")
	must(err)
	ids := s.PlanOf(plan).Created
	wss := map[string]collections.Collection{}
	review := func(name, body string) {
		ws := wss[name]
		for _, member := range ws.Members {
			if member.Name == name {
				must(os.WriteFile(filepath.Join(member.Path, "replay.sh"), []byte(body), 0644))
				gitIn(t, member.Path, "add", "replay.sh")
				gitIn(t, member.Path, "commit", "-m", "Implement replay")
			}
		}
		heads, err := b.planBaseline(ctx, ws.ID)
		must(err)
		revisions := map[string]string{}
		for _, h := range heads {
			revisions[h.Name] = h.Commit
		}
		_, err = b.Work.Record(work.Origin{Conversation: ws.ID}, work.Actor{Kind: work.ActorAgent, Role: "reviewer"}, work.Evidence{Ref: "review:" + commands.ID(), Kind: work.EvidenceReview, Workspace: ws.ID, ContractDigest: "spec-digest", Outcome: work.OutcomePass, Revisions: revisions})
		must(err)
		_, err = b.Work.Report(m.ID, ids[name], work.Human, "", "Ready")
		must(err)
	}
	for _, name := range []string{"runner", "client"} {
		wsID := commands.ID()
		ws, err := b.Collections.CreateReserved(ctx, wsID, spec)
		must(err)
		wss[name] = ws
		_, _, _, err = b.Work.Spawn(m.ID, actor, "", ids[name], name, "", work.AttemptPayload{Conversation: wsID, Execution: wsID, Workspace: wsID, Parent: actor.Conversation, Grant: grant, Profile: "codex-default", Permission: "ask", Role: "worker"})
		must(err)
	}
	review("runner", "printf 'done\\ndone\\n'\n")
	review("client", "cat\n")
	// The producer must replay a completed event after reconnect. The consumer’s
	// isolated check covers one completion. Both pass before the combined failure.
	for _, tc := range []struct{ name, check string }{{"runner", `test "$(sh runner/replay.sh | wc -w)" -eq 2`}, {"client", `test "$(echo done | sh client/replay.sh)" = done`}} {
		ws := wss[tc.name]
		cmd, err := b.Commands.Start(commands.Spec{CollectionID: ws.ID, Kind: "action", Title: "Child check", Argv: []string{"sh", "-c", tc.check}, Cwd: ws.Root})
		must(err)
		status, err := cmd.Wait(ctx)
		must(err)
		if status.ExitCode == nil || *status.ExitCode != 0 {
			t.Fatalf("%s isolated check failed", tc.name)
		}
	}
	argv := []string{"sh", "-c", `test "$(sh runner/replay.sh | sh client/replay.sh)" = done`}
	s, failed, err := b.PrepareCandidate(ctx, m.ID, "bad", plan, argv)
	must(err)
	if c := s.Candidate(failed); c.State != "failed" || c.Command == "" || s.Generation != 0 || len(s.Tasks) != 3 {
		t.Fatalf("failed check: %+v", c)
	}
	_, err = b.PromoteCandidate(ctx, m.ID, failed)
	if err == nil {
		t.Fatal("promoted failed candidate")
	}
	_, err = b.Work.Decide(m.ID, ids["client"], work.Human, "", false, "Deduplicate reconnect replay")
	must(err)
	review("client", "uniq\n")
	_, first, err := b.PrepareCandidate(ctx, m.ID, "good", plan, argv)
	must(err)
	s, same, err := b.PrepareCandidate(ctx, m.ID, "good", plan, argv)
	must(err)
	if first != same || s.Candidate(first).State != "passed" {
		t.Fatalf("retry: %+v", s.Candidate(first))
	}
	ws, err := b.Collections.Get(first)
	must(err)
	dirty := filepath.Join(ws.Members[0].Path, "dirty")
	must(os.WriteFile(dirty, []byte("pending"), 0644))
	if _, err = b.PromoteCandidate(ctx, m.ID, first); err == nil {
		t.Fatal("promoted dirty candidate")
	}
	must(os.Remove(dirty))
	_, second, err := b.PrepareCandidate(ctx, m.ID, "other", plan, argv)
	must(err)
	// Reconstruct the journal boundary after a managed check exited but before
	// its result reached the Mission. Retrying observes the original command.
	recovered := *s.Candidate(first)
	recovered.ID = commands.ID()
	recovered.State = "preparing"
	recovered.Command = ""
	recovered.Members = nil
	_, _, err = b.Work.Append(m.ID, work.Draft{Type: work.CandidateReserved, Subject: recovered.ID, Actor: work.Human, Idempotency: "interrupted", Payload: recovered})
	must(err)
	recoveredSpec := collections.Spec{Name: "Recovered check"}
	for _, member := range s.Candidate(first).Members {
		recoveredSpec.Members = append(recoveredSpec.Members, collections.MemberSpec{Name: member.Name, RepositoryID: member.Repository, BaseRef: member.Commit})
	}
	resumedWS, err := b.Collections.CreateReserved(ctx, recovered.ID, recoveredSpec)
	must(err)
	recovered.Members, err = b.planBaseline(ctx, recovered.ID)
	must(err)
	_, _, err = b.Work.Append(m.ID, work.Draft{Type: work.CandidateReady, Subject: recovered.ID, Actor: work.Actor{Kind: work.ActorLazybox}, Payload: recovered})
	must(err)
	cmd, err := b.Commands.Start(commands.Spec{CollectionID: recovered.ID, Kind: "integration", Title: "Combined check", Argv: argv, Cwd: resumedWS.Root})
	must(err)
	_, err = cmd.Wait(ctx)
	must(err)
	resumed, _, err := b.PrepareCandidate(ctx, m.ID, "interrupted", plan, argv)
	must(err)
	if c := resumed.Candidate(recovered.ID); c.State != "passed" || c.Command != cmd.Status().ID || len(b.Commands.ListWorkspace(c.ID)) != 1 {
		t.Fatalf("lost or duplicated check: %+v", c)
	}
	var wg sync.WaitGroup
	results := make(chan error, 2)
	for _, id := range []string{first, second} {
		wg.Add(1)
		go func() { defer wg.Done(); _, err := b.PromoteCandidate(ctx, m.ID, id); results <- err }()
	}
	wg.Wait()
	close(results)
	success := 0
	for err := range results {
		if err == nil {
			success++
		}
	}
	s, err = b.Work.Mission(m.ID)
	must(err)
	if success != 1 || s.Generation != 1 {
		t.Fatalf("promotions %d generation %d", success, s.Generation)
	}
	accepted := s.Candidate(s.AcceptedCandidate)
	for i, member := range base.Members {
		if got := gitIn(t, member.Path, "rev-parse", "refs/lazybox/missions/"+accepted.ID+"/"+member.Name); got != accepted.Members[i].Commit {
			t.Fatal("accepted commit not retained")
		}
	}
	for _, member := range base.Members {
		if gitIn(t, member.Path, "rev-parse", "HEAD") != member.BaseCommit {
			t.Fatal("base worktree moved")
		}
	}
}
