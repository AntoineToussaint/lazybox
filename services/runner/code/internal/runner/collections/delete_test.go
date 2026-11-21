package collections

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestDeleteWorktreesFencesStartsAndPreservesSourcesBranchesAndRetryIdentity(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, repo)
	c, err := m.Create(ctx, "delete-test", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	if _, err = m.BeginDeletion(ctx, c.ID, "wrong", false, false, inactive); err == nil {
		t.Fatal("name not checked")
	}
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err = m.Acquire(ctx, c.ID, "new-session", inactive); err == nil {
		t.Fatal("deletion did not fence attachment")
	}
	deleted, err := m.FinishDeletion(ctx, c.ID, false, inactive)
	if err != nil {
		t.Fatal(err)
	}
	if deleted.State != "deleted" || len(m.List()) != 0 {
		t.Fatal(deleted)
	}
	if _, err = os.Stat(c.Root); !os.IsNotExist(err) {
		t.Fatal("workspace remains", err)
	}
	if strings.Contains(gitTest(t, repo.Path, "worktree", "list", "--porcelain"), c.Root) {
		t.Fatal("stale Git registration")
	}
	if got := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+c.Members[0].Branch); got != c.Members[0].BaseCommit {
		t.Fatal("branch lost")
	}
	if _, err = os.Stat(filepath.Join(repo.Path, "source.txt")); err != nil {
		t.Fatal("source clone touched", err)
	}
	if _, err = m.Create(ctx, "delete-test", specTest(repo.ID)); err == nil {
		t.Fatal("deleted request resurrected workspace")
	}
	if _, err = m.FinishDeletion(ctx, c.ID, false, inactive); err != nil {
		t.Fatal("retry not idempotent", err)
	}
	m.Close()
	fresh := newTest(t, root, repo)
	if len(fresh.List()) != 0 {
		t.Fatal("restart restored deleted workspace")
	}
}
func TestDeletionRequiresConsentAndPreservesChangedFilesOnFailure(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "dirty", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	active := func(string) bool { return true }
	if _, err = m.Acquire(ctx, c.ID, "owner", inactive); err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(c.Members[0].Path, "new.txt")
	if err = os.WriteFile(file, []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(c.Root, "notes.txt"), []byte("outside worktree"), 0600); err != nil {
		t.Fatal(err)
	}
	p, err := m.PreviewDeletion(ctx, c.ID, active)
	if err != nil || len(p.Active) != 1 || len(p.Dirty) != 1 || !p.FilesOutsideRepositories {
		t.Fatal(p, err)
	}
	for _, choice := range [][2]bool{{false, false}, {true, false}, {false, true}} {
		if _, err = m.BeginDeletion(ctx, c.ID, c.Name, choice[0], choice[1], active); err == nil {
			t.Fatal("missing explicit consent accepted", choice)
		}
	}
	if _, err = os.Stat(file); err != nil {
		t.Fatal("preview/blocked delete touched files")
	}
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, true, true, active); err != nil {
		t.Fatal(err)
	}
	if _, err = m.FinishDeletion(ctx, c.ID, true, active); err == nil {
		t.Fatal("deleted with running owner")
	}
	if _, err = m.FinishDeletion(ctx, c.ID, true, inactive); err != nil {
		t.Fatal(err)
	}
}
func TestCleanupRefusesReplacedRootAndRetriesPartialDeletion(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "partial", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	gitTest(t, repo.Path, "worktree", "remove", c.Members[0].Path)
	if _, err = m.FinishDeletion(ctx, c.ID, false, inactive); err != nil {
		t.Fatal("partial deletion could not retry", err)
	}
	c, err = m.Create(ctx, "symlink", Spec{Name: "Scratch"})
	if err != nil {
		t.Fatal(err)
	}
	other := t.TempDir()
	marker := filepath.Join(other, "keep")
	os.WriteFile(marker, []byte("keep"), 0600)
	if err = os.Remove(c.Root); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(other, c.Root); err != nil {
		t.Fatal(err)
	}
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, true, true, inactive); err == nil {
		t.Fatal("followed replaced workspace root")
	}
	if _, err = os.Stat(marker); err != nil {
		t.Fatal("external file removed")
	}
}

func TestDeletionCleanupDoesNotBlockWorkspaceReads(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "slow-delete", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	if _, err := m.Acquire(ctx, c.ID, "owner", inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, false, false, inactive); err != nil {
		t.Fatal(err)
	}

	checking := make(chan struct{})
	release := make(chan struct{})
	finished := make(chan error, 1)
	go func() {
		_, err := m.FinishDeletion(ctx, c.ID, false, func(string) bool {
			close(checking)
			<-release
			return false
		})
		finished <- err
	}()
	select {
	case <-checking:
	case err := <-finished:
		t.Fatalf("cleanup exited before checking ownership: %v", err)
	}
	listed := make(chan []Collection, 1)
	go func() { listed <- m.List() }()
	select {
	case list := <-listed:
		if len(list) != 1 || list[0].State != "deleting" {
			t.Errorf("unexpected workspace list: %+v", list)
		}
	case <-time.After(time.Second):
		t.Error("workspace reads are blocked by cleanup")
	}
	close(release)
	if err := <-finished; err != nil {
		t.Fatal(err)
	}
}

func TestDeleteRepairsMissingGitLinkOnlyAfterDiscardConsent(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	workspace, err := m.Create(ctx, "missing-link", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := workspace.Members[0]
	link := filepath.Join(member.Path, ".git")
	if err := os.Remove(link); err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	preview, err := m.PreviewDeletion(ctx, workspace.ID, inactive)
	if err != nil || len(preview.Dirty) != 1 || preview.Dirty[0] != member.Name {
		t.Fatalf("preview: %+v, %v", preview, err)
	}
	if _, err := m.BeginDeletion(ctx, workspace.ID, workspace.Name, false, false, inactive); err == nil {
		t.Fatal("missing link accepted without discard consent")
	}
	if _, err := os.Lstat(link); !os.IsNotExist(err) {
		t.Fatalf("preview repaired metadata: %v", err)
	}
	if _, err := m.BeginDeletion(ctx, workspace.ID, workspace.Name, true, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.FinishDeletion(ctx, workspace.ID, true, inactive); err != nil {
		t.Fatal(err)
	}
	if got := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+member.Branch); got != member.BaseCommit {
		t.Fatal("branch lost")
	}
}

func TestDeleteUnregisteredMemberWithoutGitLink(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "orphan", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	if err := os.Remove(filepath.Join(member.Path, ".git")); err != nil {
		t.Fatal(err)
	}
	gitTest(t, repo.Path, "worktree", "prune", "--expire", "now")
	inactive := func(string) bool { return false }
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, false, false, inactive); err == nil {
		t.Fatal("discard consent required")
	}
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, true, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.FinishDeletion(ctx, c.ID, true, inactive); err != nil {
		t.Fatal(err)
	}
	if got := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+member.Branch); got != member.BaseCommit {
		t.Fatal("branch lost")
	}
}

func TestOrphanDeletionPreservesReplacementBareRepository(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "bare-replacement", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	gitTest(t, repo.Path, "worktree", "remove", member.Path)
	gitTest(t, repo.Path, "clone", "--bare", repo.Path, member.Path)
	inactive := func(string) bool { return false }
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, true, false, inactive); err == nil {
		t.Fatal("accepted deletion of a replacement bare repository")
	}
}

func TestRegisteredWorktreeBeforeLargeListingTail(t *testing.T) {
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	ctx := context.Background()
	c, err := m.Create(ctx, "large-list", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	// Git prints the lock reason in porcelain output, after the member path.
	gitTest(t, repo.Path, "worktree", "lock", "--reason", strings.Repeat("x", 70000), c.Members[0].Path)
	found, err := m.registeredWorktree(ctx, repo, c.Members[0].Path)
	if err != nil || !found {
		t.Fatalf("lost a registered worktree in a large listing: %v", err)
	}
}
